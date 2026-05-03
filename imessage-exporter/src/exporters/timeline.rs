/*!
 The Timeline exporter writes a single Markdown file that contains every
 message across every thread, grouped first by day and then by conversation
 thread. Within each thread on a given day, messages are listed in
 chronological order. Threads are not interleaved: each day shows full
 transcripts of every active thread, one after the other.
*/

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write as FmtWrite,
    fs::File,
    io::{BufWriter, Write},
};

use chrono::{DateTime, Datelike, Local, NaiveDate};

use crate::{
    app::{error::RuntimeError, export_type::ExportType, progress::ExportProgress, runtime::Config},
    exporters::exporter::Exporter,
};

use imessage_database::{
    error::table::TableError,
    message_types::variants::{Announcement, Tapback, TapbackAction, Variant},
    tables::{
        attachment::Attachment,
        chat::Chat,
        messages::{
            Message,
            models::{BubbleComponent, GroupAction},
        },
        table::{ME, ORPHANED, Table, UNKNOWN, YOU},
    },
};

/// File name (relative to the export path) of the unified timeline output.
pub(crate) const TIMELINE_FILENAME: &str = "timeline.md";

/// Heading shown when a message has no associated chat.
pub(crate) const ORPHANED_THREAD_LABEL: &str = "Orphaned messages (no associated chat)";

// MARK: ThreadKey
/// Key used to group messages by thread within a single day.
///
/// We key by the deduplicated chat id when one is available, falling back
/// to a single `Orphaned` bucket for messages that have no associated chat.
#[derive(Hash, PartialEq, Eq, Clone)]
enum ThreadKey {
    Chat(i32),
    Orphaned,
}

// MARK: ThreadEntry
/// All messages we have collected for a single (day, thread) pair, in the
/// order they were observed during message iteration.
struct ThreadEntry {
    /// Display label used as the H2 markdown heading for this thread on this day.
    display_name: String,
    /// Raw timestamp of the first message we saw on this day for this thread.
    /// Used to order threads within a day deterministically (earliest thread first).
    first_timestamp: i64,
    /// Pre-formatted message blocks in the order they appeared.
    /// Each block is already terminated with a trailing blank line so blocks
    /// concatenate cleanly into Markdown.
    messages: Vec<String>,
}

// MARK: Timeline
pub struct Timeline<'a> {
    /// Data that is set up from the application's runtime
    pub config: &'a Config,
    /// The single output file we write the unified timeline into.
    pub file: BufWriter<File>,
    /// Messages collected and grouped by day (BTreeMap so days come out in
    /// chronological order) then by thread.
    grouped: BTreeMap<NaiveDate, HashMap<ThreadKey, ThreadEntry>>,
    /// Progress bar model
    pb: ExportProgress,
}

// MARK: Exporter
impl<'a> Exporter<'a> for Timeline<'a> {
    fn new(config: &'a Config) -> Result<Self, RuntimeError> {
        let mut path = config.options.export_path.clone();
        path.push(TIMELINE_FILENAME);

        // Truncate any prior timeline.md so reruns produce a clean file.
        let file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;

        Ok(Timeline {
            config,
            file: BufWriter::new(file),
            grouped: BTreeMap::new(),
            pb: ExportProgress::new(),
        })
    }

    fn iter_messages(&mut self) -> Result<(), RuntimeError> {
        eprintln!(
            "Exporting unified timeline to {} as markdown...",
            self.config.options.export_path.display()
        );

        // Track the previous rowid so we can dedupe duplicate GUIDs the same
        // way the txt/html exporters do (see issue #135 in the upstream repo).
        let mut current_message_row = -1;

        let mut current_message = 0;
        let total_messages = Message::get_count(
            self.config.data_source.db(),
            &self.config.options.query_context,
        )?;
        self.pb.start(total_messages);

        let mut statement = Message::stream_rows(
            self.config.data_source.db(),
            &self.config.options.query_context,
        )?;

        let messages = statement
            .query_map([], |row| Ok(Message::from_row(row)))
            .map_err(|err| RuntimeError::DatabaseError(TableError::QueryError(err)))?;

        for message in messages {
            let mut msg = Message::extract(message)?;

            if msg.rowid == current_message_row {
                current_message += 1;
                continue;
            }
            current_message_row = msg.rowid;

            if let Ok(body) = msg.parse_body(self.config.data_source.db()) {
                msg.apply_body(body);
            }

            if msg.is_announcement() {
                let block = self.format_announcement(&msg);
                self.collect_message(&msg, block)?;
            } else if !msg.is_tapback() && !msg.is_poll_vote() && !msg.is_poll_update() {
                let block = self.format_timeline_message(&msg)?;
                self.collect_message(&msg, block)?;
            }

            current_message += 1;
            if current_message % 99 == 0 {
                self.pb.set_position(current_message);
            }
        }

        self.flush_grouped()?;
        self.file.flush().map_err(RuntimeError::DiskError)?;
        self.pb.finish();
        Ok(())
    }

    /// The Timeline exporter writes everything to a single file, so this
    /// always returns the same handle. It exists to satisfy the trait.
    fn get_or_create_file(
        &mut self,
        _message: &Message,
    ) -> Result<&mut BufWriter<File>, RuntimeError> {
        Ok(&mut self.file)
    }
}

// MARK: Grouping
impl<'a> Timeline<'a> {
    /// Add a fully-formatted message block to the in-memory grouping, keyed
    /// by the message's local date and thread.
    fn collect_message(&mut self, msg: &Message, block: String) -> Result<(), RuntimeError> {
        // If the date can't be parsed, skip the message rather than failing
        // the whole export. This mirrors the existing exporters' tolerance
        // for bad date data.
        let local = match msg.date(self.config.offset) {
            Ok(d) => d,
            Err(_) => return Ok(()),
        };
        let day = local.date_naive();

        let (key, display_name) = self.thread_key_and_name(msg);

        let day_bucket = self.grouped.entry(day).or_default();
        let entry = day_bucket.entry(key).or_insert_with(|| ThreadEntry {
            display_name,
            first_timestamp: msg.date,
            messages: Vec::new(),
        });
        if msg.date < entry.first_timestamp {
            entry.first_timestamp = msg.date;
        }
        entry.messages.push(block);
        Ok(())
    }

    /// Resolve the thread key and the human-readable label for a message's
    /// thread. Falls back to a single `Orphaned` bucket for messages with no
    /// associated chat.
    fn thread_key_and_name(&self, msg: &Message) -> (ThreadKey, String) {
        match self.config.conversation(msg) {
            Some((chatroom, real_id)) => (
                ThreadKey::Chat(*real_id),
                chat_display_label(self.config, chatroom),
            ),
            None => (
                ThreadKey::Orphaned,
                ORPHANED_THREAD_LABEL.to_string(),
            ),
        }
    }

    /// Write all collected messages out to the timeline file as Markdown.
    fn flush_grouped(&mut self) -> Result<(), RuntimeError> {
        let mut first_day = true;
        let days: Vec<NaiveDate> = self.grouped.keys().copied().collect();
        for day in days {
            let bucket = match self.grouped.remove(&day) {
                Some(b) => b,
                None => continue,
            };

            // Order threads within a day by their first observed timestamp;
            // ties broken by thread label so output is deterministic.
            let mut threads: Vec<ThreadEntry> = bucket.into_values().collect();
            threads.sort_by(|a, b| {
                a.first_timestamp
                    .cmp(&b.first_timestamp)
                    .then_with(|| a.display_name.cmp(&b.display_name))
            });

            if !first_day {
                self.file
                    .write_all(b"\n")
                    .map_err(RuntimeError::DiskError)?;
            }
            first_day = false;

            let header = format_day_header(&day);
            self.file
                .write_all(header.as_bytes())
                .map_err(RuntimeError::DiskError)?;

            for thread in threads {
                let thread_header = format_thread_header(&thread.display_name);
                self.file
                    .write_all(thread_header.as_bytes())
                    .map_err(RuntimeError::DiskError)?;

                for block in thread.messages {
                    self.file
                        .write_all(block.as_bytes())
                        .map_err(RuntimeError::DiskError)?;
                }
            }
        }
        Ok(())
    }
}

// MARK: Formatting
impl<'a> Timeline<'a> {
    /// Format the local time-of-day for a message as e.g. "5:29:42 PM".
    /// Returns a placeholder string on date parse error so we never lose
    /// the message entirely.
    fn format_time_of_day(&self, msg: &Message) -> String {
        match msg.date(self.config.offset) {
            Ok(d) => format_time(&d),
            Err(_) => "(unknown time)".to_string(),
        }
    }

    /// Build the full Markdown block for a single (non-announcement) message,
    /// including a header line, body, simple attachment placeholders, and
    /// any nested replies as Markdown blockquotes.
    fn format_timeline_message(&self, msg: &Message) -> Result<String, TableError> {
        let mut out = String::with_capacity(256);

        let time = self.format_time_of_day(msg);
        let sender = self.config.who(
            msg.handle_id,
            msg.is_from_me(),
            &msg.destination_caller_id,
        );

        // Header line: bold time + em-dash + bold sender.
        let _ = writeln!(out, "**{time} — {sender}**");
        out.push('\n');

        if msg.is_deleted() {
            out.push_str("*(this message was deleted from the conversation)*\n\n");
        }

        if let Some(subject) = &msg.subject
            && !subject.is_empty()
        {
            let _ = writeln!(out, "*Subject:* {subject}");
            out.push('\n');
        }

        let attachments = Attachment::from_message(self.config.data_source.db(), msg)?;
        let mut attachment_index: usize = 0;
        let mut wrote_body = false;

        // Render text and attachment components in order. We deliberately
        // keep this simpler than the TXT exporter — apps and balloons are
        // rendered as their text or a generic placeholder, which is enough
        // for the v1 timeline view.
        for component in &msg.components {
            match component {
                BubbleComponent::Text(_) => {
                    if let Some(text) = &msg.text {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            let _ = writeln!(out, "{trimmed}");
                            out.push('\n');
                            wrote_body = true;
                        }
                    }
                }
                BubbleComponent::Attachment(_) => {
                    if let Some(att) = attachments.get(attachment_index) {
                        let label = att
                            .filename()
                            .or(att.transfer_name.as_deref())
                            .unwrap_or("attachment");
                        let _ = writeln!(out, "*[Attachment: {label}]*");
                        out.push('\n');
                        wrote_body = true;
                        attachment_index += 1;
                    } else {
                        out.push_str("*[Attachment: missing]*\n\n");
                        wrote_body = true;
                    }
                }
                BubbleComponent::App => {
                    // Try to render the app's text if any; otherwise show a
                    // generic placeholder.
                    let label = msg
                        .text
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("[App message]");
                    let _ = writeln!(out, "*{label}*");
                    out.push('\n');
                    wrote_body = true;
                }
                BubbleComponent::Retracted => {
                    out.push_str("*(this part of the message was unsent)*\n\n");
                    wrote_body = true;
                }
            }
        }

        // If the components vector was empty but we have text, render it.
        if !wrote_body
            && let Some(text) = &msg.text
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let _ = writeln!(out, "{trimmed}");
                out.push('\n');
            }
        }

        // Tapbacks (reactions): show inline under the message if present.
        if let Some(tapbacks_map) = self.config.tapbacks.get(&msg.guid) {
            let mut lines: Vec<String> = Vec::new();
            for tapbacks in tapbacks_map.values() {
                for tb in tapbacks {
                    if let Some(line) = self.format_tapback_line(tb) {
                        lines.push(line);
                    }
                }
            }
            if !lines.is_empty() {
                out.push_str("Tapbacks:\n");
                for line in lines {
                    let _ = writeln!(out, "- {line}");
                }
                out.push('\n');
            }
        }

        // Replies: render each as a blockquote so the threading is visible.
        let mut replies = msg.get_replies(self.config.data_source.db())?;
        let mut reply_keys: Vec<usize> = replies.keys().copied().collect();
        reply_keys.sort_unstable();
        for k in reply_keys {
            if let Some(reply_set) = replies.get_mut(&k) {
                for reply in reply_set {
                    if let Ok(body) = reply.parse_body(self.config.data_source.db()) {
                        reply.apply_body(body);
                    }
                    if reply.is_tapback() || reply.is_poll_vote() || reply.is_poll_update() {
                        continue;
                    }
                    let nested = self.format_timeline_message(reply)?;
                    out.push_str(&blockquote(&nested));
                    out.push('\n');
                }
            }
        }

        Ok(out)
    }

    /// Format a single tapback (reaction) into a line like
    /// `Loved by Sample Contact` or `Sticker from Me`.
    fn format_tapback_line(&self, msg: &Message) -> Option<String> {
        if let Variant::Tapback(_, action, tapback) = msg.variant() {
            if matches!(action, TapbackAction::Removed) {
                return None;
            }
            let who = self.config.who(
                msg.handle_id,
                msg.is_from_me(),
                &msg.destination_caller_id,
            );
            return Some(match tapback {
                Tapback::Sticker => format!("Sticker from {who}"),
                _ => format!("{tapback} by {who}"),
            });
        }
        None
    }

    /// Build the Markdown block for an announcement (e.g. group rename, kept
    /// audio message). Rendered as a single italic line so it's visually
    /// distinct from regular messages.
    fn format_announcement(&self, msg: &Message) -> String {
        let mut who = self
            .config
            .who(msg.handle_id, msg.is_from_me(), &msg.destination_caller_id);
        if who == ME {
            who = self.config.options.custom_name.as_deref().unwrap_or(YOU);
        }

        let time = self.format_time_of_day(msg);

        let action_text = match msg.get_announcement() {
            Some(announcement) => match announcement {
                Announcement::GroupAction(action) => match action {
                    GroupAction::ParticipantAdded(person)
                    | GroupAction::ParticipantRemoved(person) => {
                        let resolved =
                            self.config
                                .who(Some(person), false, &msg.destination_caller_id);
                        let (verb, prep) =
                            if matches!(action, GroupAction::ParticipantAdded(_)) {
                                ("added", "to")
                            } else {
                                ("removed", "from")
                            };
                        format!("{verb} {resolved} {prep} the conversation.")
                    }
                    GroupAction::NameChange(name) => {
                        format!("renamed the conversation to {name}")
                    }
                    GroupAction::ParticipantLeft => "left the conversation.".to_string(),
                    GroupAction::GroupIconChanged => "changed the group photo.".to_string(),
                    GroupAction::GroupIconRemoved => "removed the group photo.".to_string(),
                    GroupAction::ChatBackgroundChanged => {
                        "changed the chat background.".to_string()
                    }
                    GroupAction::ChatBackgroundRemoved => {
                        "removed the chat background.".to_string()
                    }
                    GroupAction::PhoneNumberChanged(_) => {
                        "changed their phone number.".to_string()
                    }
                },
                Announcement::AudioMessageKept => "kept an audio message.".to_string(),
                Announcement::FullyUnsent => "unsent a message!".to_string(),
                Announcement::Unknown(num) => format!("performed unknown action {num}"),
            },
            None => "(unable to format announcement)".to_string(),
        };

        format!("*{time} — {who} {action_text}*\n\n")
    }
}

// MARK: Free helpers
/// Format a `DateTime<Local>` as "5:29:42 PM" (matches the time portion of
/// the existing `dates::format` style: `%l:%M:%S %p`).
pub(crate) fn format_time(date: &DateTime<Local>) -> String {
    DateTime::format(date, "%l:%M:%S %p")
        .to_string()
        .trim_start()
        .to_string()
}

/// Build the H1 day header, e.g. "# Saturday, March 21, 2026\n\n".
pub(crate) fn format_day_header(date: &NaiveDate) -> String {
    // Manually format to avoid platform-specific differences in chrono's
    // locale-aware specifiers.
    let weekday = match date.weekday() {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    };
    let month = match date.month() {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "?",
    };
    format!("# {weekday}, {month} {}, {}\n\n", date.day(), date.year())
}

/// Build the H2 thread header for a single thread on a single day.
pub(crate) fn format_thread_header(name: &str) -> String {
    format!("## {name}\n\n")
}

/// Prefix every line of `text` with `> ` for a Markdown blockquote.
fn blockquote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for line in text.split_inclusive('\n') {
        if line == "\n" {
            out.push_str(">\n");
        } else {
            out.push_str("> ");
            out.push_str(line);
        }
    }
    // Ensure trailing newline.
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Compute the chat label used as the H2 heading for a thread.
///
/// Reuses `Config::filename` to pick up the same display-name-vs-participants
/// logic the per-chat exporters use, then strips the trailing extension
/// (e.g. ".md") so the heading reads naturally.
pub(crate) fn chat_display_label(config: &Config, chatroom: &Chat) -> String {
    let mut name = config.filename(chatroom);
    let extensions: [&str; 3] = [
        ExportType::Timeline.extension(),
        ExportType::Txt.extension(),
        ExportType::Html.extension(),
    ];
    for ext in extensions {
        if let Some(stripped) = name.strip_suffix(ext) {
            name = stripped.to_string();
            break;
        }
    }
    if name.is_empty() {
        if !chatroom.chat_identifier.is_empty() {
            return chatroom.chat_identifier.clone();
        }
        return UNKNOWN.to_string();
    }
    // Fix the legacy ORPHANED constant leaking through if somehow used.
    if name == ORPHANED {
        return ORPHANED_THREAD_LABEL.to_string();
    }
    name
}

// MARK: Tests
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Config, Exporter, Options,
        app::{contacts::Name, export_type::ExportType},
    };
    use chrono::NaiveDate;
    use imessage_database::tables::{chat::Chat, table::ME};

    fn fake_chat() -> Chat {
        Chat {
            rowid: 0,
            chat_identifier: "Default".to_string(),
            service_name: Some(String::new()),
            display_name: None,
        }
    }

    #[test]
    fn day_header_formats_full_weekday_and_month() {
        let date = NaiveDate::from_ymd_opt(2026, 3, 21).unwrap();
        assert_eq!(format_day_header(&date), "# Saturday, March 21, 2026\n\n");
    }

    #[test]
    fn day_header_no_zero_padding_on_day() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        assert_eq!(format_day_header(&date), "# Monday, January 5, 2026\n\n");
    }

    #[test]
    fn thread_header_uses_h2() {
        assert_eq!(
            format_thread_header("Andrew"),
            "## Andrew\n\n".to_string()
        );
    }

    #[test]
    fn blockquote_prefixes_each_line() {
        let input = "first\nsecond\n";
        let expected = "> first\n> second\n";
        assert_eq!(blockquote(input), expected);
    }

    #[test]
    fn blockquote_keeps_blank_lines_as_empty_quote() {
        let input = "first\n\nthird\n";
        let expected = "> first\n>\n> third\n";
        assert_eq!(blockquote(input), expected);
    }

    #[test]
    fn can_create_timeline_exporter() {
        let _ = std::fs::remove_file("/tmp/timeline.md");
        let options = Options::fake_options(ExportType::Timeline);
        let config = Config::fake_app(options);
        let exporter = Timeline::new(&config).unwrap();
        // No grouped data and no orphaned messages have been seen yet.
        assert!(exporter.grouped.is_empty());
    }

    #[test]
    fn chat_display_label_uses_display_name() {
        let options = Options::fake_options(ExportType::Timeline);
        let app = Config::fake_app(options);
        let mut chat = fake_chat();
        chat.display_name = Some("Test Chat Name".to_string());
        let label = chat_display_label(&app, &chat);
        // Filename adds " - <id>.md"; we strip the .md but keep the id suffix.
        assert_eq!(label, "Test Chat Name - 0");
    }

    #[test]
    fn chat_display_label_falls_back_to_chat_identifier() {
        let options = Options::fake_options(ExportType::Timeline);
        let app = Config::fake_app(options);
        let chat = fake_chat();
        let label = chat_display_label(&app, &chat);
        assert_eq!(label, "Default");
    }

    #[test]
    fn format_timeline_message_basic_from_me() {
        let _ = std::fs::remove_file("/tmp/timeline.md");
        let options = Options::fake_options(ExportType::Timeline);
        let config = Config::fake_app(options);
        let exporter = Timeline::new(&config).unwrap();

        let mut message = Config::fake_message();
        // May 17, 2022 8:29:42 PM UTC -> 5:29:42 PM PT
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let actual = exporter.format_timeline_message(&message).unwrap();
        let expected = "**5:29:42 PM — Me**\n\nHello world\n\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn format_timeline_message_basic_from_them() {
        let _ = std::fs::remove_file("/tmp/timeline.md");
        let options = Options::fake_options(ExportType::Timeline);
        let mut config = Config::fake_app(options);
        config
            .participants
            .insert(999_999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999_999, 999_999);
        let exporter = Timeline::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        message.handle_id = Some(999_999);
        message
            .generate_text_legacy(config.data_source.db())
            .unwrap();

        let actual = exporter.format_timeline_message(&message).unwrap();
        let expected = "**5:29:42 PM — Sample Contact**\n\nHello world\n\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn format_announcement_renames_conversation() {
        let _ = std::fs::remove_file("/tmp/timeline.md");
        let options = Options::fake_options(ExportType::Timeline);
        let mut config = Config::fake_app(options);
        config.participants.insert(0, Name::fake_name(ME));
        let exporter = Timeline::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;
        message.item_type = 2;

        let actual = exporter.format_announcement(&message);
        let expected = "*5:29:42 PM — You renamed the conversation to Hello world*\n\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn collect_message_groups_by_day_and_thread() {
        let _ = std::fs::remove_file("/tmp/timeline.md");
        let options = Options::fake_options(ExportType::Timeline);
        let mut config = Config::fake_app(options);
        // Provide a chat so messages with chat_id=0 resolve to a thread.
        config.chatrooms.insert(0, fake_chat());
        config.real_chatrooms.insert(0, 0);
        let mut exporter = Timeline::new(&config).unwrap();

        let mut msg_a = Config::fake_message();
        msg_a.date = 674526582885055488; // May 17, 2022 5:29:42 PM PT
        msg_a.text = Some("first".to_string());
        msg_a.is_from_me = true;
        msg_a.chat_id = Some(0);

        let mut msg_b = Config::fake_message();
        msg_b.date = 674530231992568192; // May 17, 2022 6:30:31 PM PT - same day
        msg_b.text = Some("second".to_string());
        msg_b.is_from_me = true;
        msg_b.chat_id = Some(0);

        exporter
            .collect_message(&msg_a, "block-a\n".to_string())
            .unwrap();
        exporter
            .collect_message(&msg_b, "block-b\n".to_string())
            .unwrap();

        // Both messages should land under the same day, same thread.
        assert_eq!(exporter.grouped.len(), 1);
        let day = exporter.grouped.keys().next().copied().unwrap();
        let bucket = exporter.grouped.get(&day).unwrap();
        assert_eq!(bucket.len(), 1);
        let entry = bucket.values().next().unwrap();
        assert_eq!(entry.messages.len(), 2);
        assert_eq!(entry.first_timestamp, 674526582885055488);
    }

    #[test]
    fn flush_grouped_emits_day_then_thread_then_messages() {
        let tmp_dir = tempdir_for_test("timeline_flush");
        let mut options = Options::fake_options(ExportType::Timeline);
        options.export_path = tmp_dir.clone();
        let mut config = Config::fake_app(options);
        config.chatrooms.insert(0, {
            let mut c = fake_chat();
            c.display_name = Some("Andrew".to_string());
            c
        });
        config.real_chatrooms.insert(0, 0);
        let mut exporter = Timeline::new(&config).unwrap();

        let mut msg = Config::fake_message();
        msg.date = 674526582885055488; // May 17, 2022 in PT
        msg.text = Some("hi".to_string());
        msg.is_from_me = true;
        msg.chat_id = Some(0);

        let block = "**5:29:42 PM — Me**\n\nhi\n\n".to_string();
        exporter.collect_message(&msg, block).unwrap();

        exporter.flush_grouped().unwrap();
        exporter.file.flush().unwrap();
        drop(exporter);

        let path = tmp_dir.join(TIMELINE_FILENAME);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.starts_with("# Tuesday, May 17, 2022\n\n"),
            "expected day header at start, got:\n{contents}"
        );
        assert!(
            contents.contains("## Andrew - 0\n\n"),
            "expected thread header, got:\n{contents}"
        );
        assert!(
            contents.contains("**5:29:42 PM — Me**\n\nhi\n\n"),
            "expected formatted message, got:\n{contents}"
        );
    }

    #[test]
    fn flush_grouped_orders_threads_by_first_timestamp_within_day() {
        let tmp_dir = tempdir_for_test("timeline_thread_order");
        let mut options = Options::fake_options(ExportType::Timeline);
        options.export_path = tmp_dir.clone();
        let mut config = Config::fake_app(options);
        // Two chats, each with a distinct display name.
        let mut early_chat = fake_chat();
        early_chat.rowid = 1;
        early_chat.display_name = Some("EarlyThread".to_string());
        let mut late_chat = fake_chat();
        late_chat.rowid = 2;
        late_chat.display_name = Some("LateThread".to_string());
        config.chatrooms.insert(1, early_chat);
        config.chatrooms.insert(2, late_chat);
        config.real_chatrooms.insert(1, 1);
        config.real_chatrooms.insert(2, 2);

        let mut exporter = Timeline::new(&config).unwrap();

        // Insert the LATE thread first to ensure ordering doesn't depend on
        // insertion order.
        let mut msg_late = Config::fake_message();
        msg_late.date = 674530231992568192; // 6:30 PM
        msg_late.text = Some("late".to_string());
        msg_late.is_from_me = true;
        msg_late.chat_id = Some(2);

        let mut msg_early = Config::fake_message();
        msg_early.date = 674526582885055488; // 5:29 PM
        msg_early.text = Some("early".to_string());
        msg_early.is_from_me = true;
        msg_early.chat_id = Some(1);

        exporter
            .collect_message(&msg_late, "LATE-BLOCK\n".to_string())
            .unwrap();
        exporter
            .collect_message(&msg_early, "EARLY-BLOCK\n".to_string())
            .unwrap();

        exporter.flush_grouped().unwrap();
        exporter.file.flush().unwrap();
        drop(exporter);

        let path = tmp_dir.join(TIMELINE_FILENAME);
        let contents = std::fs::read_to_string(&path).unwrap();

        let early_pos = contents.find("EarlyThread").expect("missing EarlyThread");
        let late_pos = contents.find("LateThread").expect("missing LateThread");
        assert!(
            early_pos < late_pos,
            "EarlyThread should appear before LateThread; got:\n{contents}"
        );

        let early_block_pos = contents
            .find("EARLY-BLOCK")
            .expect("missing EARLY-BLOCK");
        let late_block_pos = contents.find("LATE-BLOCK").expect("missing LATE-BLOCK");
        assert!(early_block_pos < late_block_pos);
    }

    #[test]
    fn flush_grouped_emits_days_in_chronological_order() {
        let tmp_dir = tempdir_for_test("timeline_day_order");
        let mut options = Options::fake_options(ExportType::Timeline);
        options.export_path = tmp_dir.clone();
        let mut config = Config::fake_app(options);
        config.chatrooms.insert(0, {
            let mut c = fake_chat();
            c.display_name = Some("Andrew".to_string());
            c
        });
        config.real_chatrooms.insert(0, 0);
        let mut exporter = Timeline::new(&config).unwrap();

        // Day 2 message first, then day 1 message, to verify sort.
        let mut msg_day2 = Config::fake_message();
        msg_day2.date = 674526582885055488 + 86_400_000_000_000; // +1 day
        msg_day2.text = Some("day2".to_string());
        msg_day2.is_from_me = true;
        msg_day2.chat_id = Some(0);

        let mut msg_day1 = Config::fake_message();
        msg_day1.date = 674526582885055488;
        msg_day1.text = Some("day1".to_string());
        msg_day1.is_from_me = true;
        msg_day1.chat_id = Some(0);

        exporter
            .collect_message(&msg_day2, "DAY2-BLOCK\n".to_string())
            .unwrap();
        exporter
            .collect_message(&msg_day1, "DAY1-BLOCK\n".to_string())
            .unwrap();

        exporter.flush_grouped().unwrap();
        exporter.file.flush().unwrap();
        drop(exporter);

        let path = tmp_dir.join(TIMELINE_FILENAME);
        let contents = std::fs::read_to_string(&path).unwrap();

        let day1_pos = contents.find("DAY1-BLOCK").expect("missing DAY1-BLOCK");
        let day2_pos = contents.find("DAY2-BLOCK").expect("missing DAY2-BLOCK");
        assert!(
            day1_pos < day2_pos,
            "expected day 1 to come before day 2; got:\n{contents}"
        );

        // There should be two day headers.
        let header_count = contents.matches("# ").count();
        // (The thread headers start with "## " so they don't double-count
        // since `matches("# ")` only matches "# " exactly when followed by
        // non-#; but to be safe, count the H1 lines explicitly.)
        let h1_count = contents
            .lines()
            .filter(|l| l.starts_with("# ") && !l.starts_with("## "))
            .count();
        assert_eq!(h1_count, 2, "expected 2 H1 day headers; got {header_count}");
    }

    fn tempdir_for_test(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("imessage_timeline_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
