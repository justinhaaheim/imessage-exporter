/*!
 The Timeline exporter writes a single Markdown file that contains every
 message across every thread, grouped first by day and then by conversation
 thread. Within each thread on a given day, messages are listed in
 chronological order. Threads are not interleaved: each day shows full
 transcripts of every active thread, one after the other.
*/

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    fmt::Write as FmtWrite,
    fs::File,
    io::{BufWriter, IsTerminal, Write, stderr},
};

use chrono::{DateTime, Datelike, NaiveDate};

use crate::{
    app::{
        error::RuntimeError,
        export_type::ExportType,
        progress::ExportProgress,
        runtime::{Config, ORPHANED, UNKNOWN},
        zone::Zone,
    },
    exporters::{
        shared::{
            announcement::{YOU, resolve_announcement},
            balloon::rewrite_fitness_receiver,
        },
        timeline_balloons::render_app_balloon,
    },
};

use imessage_database::{
    error::table::TableError,
    message_types::variants::{Announcement, KeptMessage, Tapback, TapbackAction, Variant},
    tables::{
        attachment::Attachment,
        chat::Chat,
        messages::{
            Message,
            models::{AttributedRange, BubbleComponent, GroupAction},
        },
    },
};

/// File name (relative to the export path) of the unified timeline output.
pub(crate) const TIMELINE_FILENAME: &str = "timeline.md";

/// Heading shown when a message has no associated chat.
pub(crate) const ORPHANED_THREAD_LABEL: &str = "Orphaned messages (no associated chat)";

/// Shown after a message part that was edited. The timeline shows only the
/// latest text; the TXT export shows the full edit history.
pub(crate) const EDITED_MARKER: &str = "*(edited)*";

/// Shown for an app message whose balloon cannot be rendered and that has no text.
pub(crate) const APP_MESSAGE_PLACEHOLDER: &str = "[App message]";

/// U+FFFC, which Messages puts in a message's text where an attachment or an
/// app balloon sits.
const OBJECT_REPLACEMENT_CHAR: char = '\u{FFFC}';

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

// MARK: ThreadLabels
/// Thread headings already computed, keyed by chat rowid.
///
/// The heading comes from upstream's `Config::filename`, which prints
/// "Found error: message chat ID N has no members!" each time it runs for a
/// chat with no participants. The TXT and HTML exporters call it once per
/// chat, so the timeline caches the heading and calls it once per chat too
/// (djf.5). Keyed by the chat row passed to `Config::filename`, so every
/// heading is exactly what an uncached call would return.
#[derive(Default)]
struct ThreadLabels {
    by_chat: HashMap<i32, String>,
    /// How many headings were computed instead of read from the cache.
    #[cfg(test)]
    computed: usize,
}

impl ThreadLabels {
    /// The cached heading for `chat_rowid`, computing it with `compute` the
    /// first time the chat is seen.
    fn get_or_compute(&mut self, chat_rowid: i32, compute: impl FnOnce() -> String) -> String {
        self.by_chat
            .entry(chat_rowid)
            .or_insert_with(|| {
                #[cfg(test)]
                {
                    self.computed += 1;
                }
                compute()
            })
            .clone()
    }
}

// MARK: Timeline
pub struct Timeline<'a> {
    /// Data that is set up from the application's runtime
    pub config: &'a Config,
    /// The single output file we write the unified timeline into.
    pub file: BufWriter<File>,
    /// Messages collected and grouped by day (BTreeMap so days come out in
    /// chronological order) then by thread.
    ///
    /// TODO: this buffers every formatted block until iteration finishes.
    /// For very large databases that's a lot of RAM; a streaming pass
    /// (sort by local-day at the SQL layer, flush per-day) would scale
    /// better.
    grouped: BTreeMap<NaiveDate, HashMap<ThreadKey, ThreadEntry>>,
    /// Thread headings computed so far, one per chat.
    thread_labels: ThreadLabels,
    /// Progress bar model
    pb: ExportProgress,
}

// MARK: Exporter
impl<'a> Timeline<'a> {
    pub fn new(config: &'a Config) -> Result<Self, RuntimeError> {
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
            thread_labels: ThreadLabels::default(),
            pb: ExportProgress::new(config.options.show_progress && stderr().is_terminal()),
        })
    }

    pub fn iter_messages(&mut self) -> Result<(), RuntimeError> {
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
            &self.config.data_source.capabilities,
            &self.config.options.query_context,
        )?;
        self.pb.start(total_messages);

        let mut statement = Message::stream_rows(
            self.config.data_source.db(),
            &self.config.data_source.capabilities,
            &self.config.options.query_context,
        )?;

        // Message bodies (text and components) are parsed while each row is
        // read, so there is no separate body-parsing step here.
        for message in Message::rows(&mut statement, [])? {
            let msg = message?;

            if msg.rowid == current_message_row {
                current_message += 1;
                continue;
            }
            current_message_row = msg.rowid;

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
}

// MARK: Grouping
impl<'a> Timeline<'a> {
    /// Add a fully-formatted message block to the in-memory grouping, keyed
    /// by the message's local date and thread.
    fn collect_message(&mut self, msg: &Message, block: String) -> Result<(), RuntimeError> {
        // If the date can't be parsed, skip the message rather than failing
        // the whole export. This mirrors the existing exporters' tolerance
        // for bad date data.
        let local = match msg.date() {
            Ok(d) => d.with_timezone(&self.config.options.timezone),
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
    /// associated chat. The label is computed once per chat (djf.5).
    fn thread_key_and_name(&mut self, msg: &Message) -> (ThreadKey, String) {
        let config = self.config;
        match config.conversation(msg) {
            Some((chatroom, real_id)) => (
                ThreadKey::Chat(*real_id),
                self.thread_labels
                    .get_or_compute(chatroom.rowid, || chat_display_label(config, chatroom)),
            ),
            None => (ThreadKey::Orphaned, ORPHANED_THREAD_LABEL.to_string()),
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
        match msg.date() {
            Ok(d) => format_time(&d.with_timezone(&self.config.options.timezone)),
            Err(_) => "(unknown time)".to_string(),
        }
    }

    /// Build the full Markdown block for a single (non-announcement) message,
    /// including a header line, body, simple attachment placeholders, and
    /// any nested replies as Markdown blockquotes.
    fn format_timeline_message(&self, msg: &Message) -> Result<String, TableError> {
        let mut out = String::with_capacity(256);

        let time = self.format_time_of_day(msg);
        let sender = self
            .config
            .who(msg.handle_id, msg.is_from_me(), &msg.destination_caller_id);

        // Header line: bold time + em-dash + bold sender.
        let _ = writeln!(out, "**{time} — {sender}**");
        out.push('\n');

        if msg.is_deleted() {
            out.push_str("*(this message was deleted from the conversation)*\n\n");
        }

        if let Some(subject) = &msg.subject
            && !subject.is_empty()
        {
            let subject = escape_heading_lines(subject);
            let _ = writeln!(out, "*Subject:* {subject}");
            out.push('\n');
        }

        let attachments = Attachment::from_message(
            self.config.data_source.db(),
            msg,
            &self.config.data_source.capabilities,
        )?;
        let mut attachment_index: usize = 0;
        let mut wrote_body = false;

        // Render text, attachment and app components in order. Text and
        // attachments stay simpler than the TXT exporter (attachments are
        // named, not embedded); app balloons show the same fields TXT shows.
        for (part_index, component) in msg.components.iter().enumerate() {
            match component {
                BubbleComponent::Run(ranges) => {
                    // Render text ranges and inline attachment ranges in order.
                    let text_ranges: Vec<AttributedRange> = ranges
                        .iter()
                        .filter(|r| r.attachment.is_none())
                        .cloned()
                        .collect();
                    let has_attachments = ranges.iter().any(|r| r.attachment.is_some());
                    // djf.2: the timeline shows only the latest text of an
                    // edited part, so say that it was edited.
                    let edited = msg.is_part_edited(part_index);
                    let mut marked_edited = false;

                    if let Some(text) = &msg.text {
                        // As TXT does for text bubbles, read Fitness's
                        // receiver placeholder as "You".
                        let slice =
                            rewrite_fitness_receiver(slice_text_for_bubble(text, &text_ranges));
                        let trimmed = slice.trim();
                        if !trimmed.is_empty() {
                            // djf.9: no line of the text may read as a heading.
                            let trimmed = escape_heading_lines(trimmed);
                            if edited {
                                let _ = writeln!(out, "{trimmed} {EDITED_MARKER}");
                                marked_edited = true;
                            } else {
                                let _ = writeln!(out, "{trimmed}");
                            }
                            out.push('\n');
                            wrote_body = true;
                        }
                    }

                    if edited && !marked_edited {
                        let _ = writeln!(out, "{EDITED_MARKER}");
                        out.push('\n');
                        wrote_body = true;
                    }

                    if has_attachments {
                        if let Some(att) = attachments.get(attachment_index) {
                            // djf.8: name a sticker as a sticker, as TXT does.
                            let (kind, fallback) = if att.is_sticker {
                                ("Sticker", "sticker")
                            } else {
                                ("Attachment", "attachment")
                            };
                            let label = att
                                .filename()
                                .or(att.transfer_name.as_deref())
                                .unwrap_or(fallback);
                            let _ = writeln!(out, "*[{kind}: {label}]*");
                            out.push('\n');
                            wrote_body = true;
                            attachment_index += 1;
                        } else {
                            out.push_str("*[Attachment: missing]*\n\n");
                            wrote_body = true;
                        }
                    }
                }
                BubbleComponent::App => {
                    // djf.2: render the balloon through upstream's decision
                    // tree. If the payload is missing or unreadable, keep the
                    // output the timeline had before balloons were rendered,
                    // so one bad balloon never fails the export.
                    let block = render_app_balloon(self.config, msg, &attachments)
                        .unwrap_or_else(|_| app_message_fallback(msg));
                    let _ = writeln!(out, "{block}");
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
        if !wrote_body && let Some(text) = &msg.text {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let trimmed = escape_heading_lines(trimmed);
                let _ = writeln!(out, "{trimmed}");
                out.push('\n');
            }
        }

        // Tapbacks (reactions): show inline under the message if present.
        // Iterate component indices in sorted order so output is deterministic
        // — `tapbacks_map` is a HashMap, which would otherwise yield a
        // run-to-run ordering.
        if let Some(tapbacks_map) = self.config.tapbacks.get(&msg.guid) {
            let mut tapback_keys: Vec<usize> = tapbacks_map.keys().copied().collect();
            tapback_keys.sort_unstable();
            let mut lines: Vec<String> = Vec::new();
            for k in tapback_keys {
                if let Some(tapbacks) = tapbacks_map.get(&k) {
                    for tb in tapbacks {
                        if let Some(line) = self.format_tapback_line(tb) {
                            lines.push(line);
                        }
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
        let replies = msg.get_replies(
            self.config.data_source.db(),
            &self.config.data_source.capabilities,
        )?;
        let mut reply_keys: Vec<usize> = replies.keys().copied().collect();
        reply_keys.sort_unstable();
        for k in reply_keys {
            if let Some(reply_set) = replies.get(&k) {
                for reply in reply_set {
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
            let who = self
                .config
                .who(msg.handle_id, msg.is_from_me(), &msg.destination_caller_id);
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
    ///
    /// The actor and the added or removed participant come from upstream's
    /// shared `resolve_announcement`, so the timeline names the same person
    /// the TXT and HTML exports name. That includes events Messages shows
    /// with no actor (unattributed group events, unknown event rows, locations
    /// sent to emergency services), which get a sentence without a subject.
    fn format_announcement(&self, msg: &Message) -> String {
        let time = self.format_time_of_day(msg);

        let Some(resolved) = resolve_announcement(msg, self.config, YOU) else {
            return format!("*{time} — (unable to format announcement)*\n\n");
        };
        let participant = resolved.participant_name;

        let sentence = match resolved.who {
            Some(who) => {
                let action_text = match &resolved.announcement {
                    Announcement::GroupAction(action) => match action {
                        GroupAction::ParticipantAdded(_) => {
                            format!("added {participant} to the conversation.")
                        }
                        GroupAction::ParticipantRemoved(_) => {
                            format!("removed {participant} from the conversation.")
                        }
                        GroupAction::NameChange(name) => {
                            format!("renamed the conversation to {name}.")
                        }
                        GroupAction::NameRemoved => {
                            "removed the name from the conversation.".to_string()
                        }
                        GroupAction::ParticipantLeft => "left the conversation.".to_string(),
                        GroupAction::GroupIconChanged | GroupAction::GroupIconChangedHidden => {
                            "changed the group photo.".to_string()
                        }
                        GroupAction::GroupIconRemoved => "removed the group photo.".to_string(),
                        GroupAction::ChatBackgroundChanged
                        | GroupAction::ChatBackgroundChangedHidden => {
                            "changed the chat background.".to_string()
                        }
                        GroupAction::ChatBackgroundSensitive => {
                            "set a chat background which may be sensitive.".to_string()
                        }
                        GroupAction::ChatBackgroundRemoved => {
                            "removed the chat background.".to_string()
                        }
                        GroupAction::PhoneNumberChanged(_) => {
                            "changed their phone number.".to_string()
                        }
                    },
                    Announcement::MessageKept(KeptMessage::Audio) => {
                        "kept an audio message.".to_string()
                    }
                    Announcement::MessageKept(KeptMessage::Video) => {
                        "kept a video message.".to_string()
                    }
                    Announcement::MessageKept(KeptMessage::App | KeptMessage::Unknown(_)) => {
                        "kept a message.".to_string()
                    }
                    Announcement::FullyUnsent => "unsent a message!".to_string(),
                    Announcement::TranscriptSharingStopped => {
                        "stopped sharing the Emergency SOS transcript of this conversation."
                            .to_string()
                    }
                    // `resolve_announcement` never gives these an actor.
                    Announcement::EmergencyLocationSent | Announcement::UnknownEvent(_) => {
                        "(unable to format announcement)".to_string()
                    }
                };
                format!("{who} {action_text}")
            }
            None => match &resolved.announcement {
                Announcement::GroupAction(action) => match action {
                    GroupAction::ParticipantAdded(_) => {
                        format!("{participant} was added to the conversation.")
                    }
                    GroupAction::ParticipantRemoved(_) => {
                        format!("{participant} was removed from the conversation.")
                    }
                    GroupAction::NameChange(name) => {
                        format!("The conversation was renamed to {name}.")
                    }
                    GroupAction::NameRemoved => "The conversation name was removed.".to_string(),
                    GroupAction::GroupIconChanged | GroupAction::GroupIconChangedHidden => {
                        "The group photo was changed.".to_string()
                    }
                    GroupAction::GroupIconRemoved => "The group photo was removed.".to_string(),
                    GroupAction::ChatBackgroundChanged
                    | GroupAction::ChatBackgroundChangedHidden => {
                        "The chat background was changed.".to_string()
                    }
                    GroupAction::ChatBackgroundSensitive => {
                        "A chat background was set which may be sensitive.".to_string()
                    }
                    GroupAction::ChatBackgroundRemoved => {
                        "The chat background was removed.".to_string()
                    }
                    // `resolve_announcement` always gives these an actor.
                    GroupAction::ParticipantLeft | GroupAction::PhoneNumberChanged(_) => {
                        "(unable to format announcement)".to_string()
                    }
                },
                Announcement::EmergencyLocationSent => {
                    "Location sent to emergency services.".to_string()
                }
                Announcement::UnknownEvent(event) => format!("Unknown event ({event:?})."),
                Announcement::MessageKept(_)
                | Announcement::FullyUnsent
                | Announcement::TranscriptSharingStopped => {
                    "(unable to format announcement)".to_string()
                }
            },
        };

        format!("*{time} — {sentence}*\n\n")
    }
}

// MARK: Free helpers
/// Format a `DateTime` in the export's timezone as "5:29:42 PM". Uses `%-l`
/// (unpadded 12-hour) so we don't need to trim a leading space the way the
/// `imessage_database::util::dates::format` helper would.
pub(crate) fn format_time(date: &DateTime<Zone>) -> String {
    date.format("%-l:%M:%S %p").to_string()
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

/// Concatenate just the slices of `text` belonging to a single Text bubble's
/// attributes. Without this, multi-bubble messages render the full message
/// body once per Text bubble. Mirrors `TXT::format_attributes` (de-dupes
/// adjacent runs with the same range and ignores out-of-range attributes).
fn slice_text_for_bubble(text: &str, attributes: &[AttributedRange]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_start = 0;
    let mut prev_end = 0;
    for effect in attributes {
        if prev_start == effect.start && prev_end == effect.end {
            continue;
        }
        if let Some(piece) = text.get(effect.start..effect.end) {
            prev_start = effect.start;
            prev_end = effect.end;
            out.push_str(piece);
        }
    }
    out
}

/// The timeline's output for an app message before balloons were rendered:
/// the message text in italics, or `*[App message]*` when there is none.
/// Used when upstream's balloon dispatcher returns an error.
///
/// Text made only of object-replacement characters and whitespace counts as
/// none (djf.10): an app message's text is often a lone U+FFFC, which would
/// otherwise print as an italic invisible character.
fn app_message_fallback(msg: &Message) -> String {
    let label = msg
        .text
        .as_deref()
        .map(str::trim)
        .filter(|s| {
            !s.chars()
                .all(|c| c == OBJECT_REPLACEMENT_CHAR || c.is_whitespace())
        })
        .unwrap_or(APP_MESSAGE_PLACEHOLDER);
    // djf.9: a later line of a multi-line text could read as a heading.
    format!("*{}*", escape_heading_lines(label))
}

/// Escape every line of `text` that CommonMark would read as an ATX heading
/// (djf.9), so message text never looks like a day or thread heading.
///
/// A heading line is up to three spaces, then one to six `#`, then a space,
/// a tab or the end of the line. A backslash goes before its first `#`, which
/// Markdown renders as a plain `#`. Hashtags such as `#blessed` (no space
/// after the `#`), seven or more `#`, and lines indented four or more spaces
/// are left alone, because none of them is a heading. Lines end at `\n` or
/// `\r`, as in CommonMark. Escaping an escaped line changes nothing.
pub(crate) fn escape_heading_lines(text: &str) -> Cow<'_, str> {
    let mut escaped: Option<String> = None;
    let mut copied = 0;
    let mut line_start = 0;
    loop {
        let rest = &text[line_start..];
        if let Some(indent) = heading_marker_indent(rest) {
            let hash_at = line_start + indent;
            let out = escaped.get_or_insert_with(|| String::with_capacity(text.len() + 8));
            out.push_str(&text[copied..hash_at]);
            out.push('\\');
            copied = hash_at;
        }
        match rest.find(['\n', '\r']) {
            Some(end) => line_start += end + 1,
            None => break,
        }
    }
    match escaped {
        Some(mut out) => {
            out.push_str(&text[copied..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(text),
    }
}

/// The number of leading spaces when `line` starts with an ATX heading
/// marker, else `None`. `line` may run past its own line ending.
fn heading_marker_indent(line: &str) -> Option<usize> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 {
        return None;
    }
    let after_indent = &line.as_bytes()[indent..];
    let hashes = after_indent.iter().take_while(|&&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    match after_indent.get(hashes) {
        None | Some(b' ' | b'\t' | b'\n' | b'\r') => Some(indent),
        Some(_) => None,
    }
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
    use std::sync::OnceLock;

    use super::*;
    use crate::{
        Config, Options,
        app::{contacts::Name, export_type::ExportType, test_dir::unique_test_dir},
        exporters::shared::test_support::SAMPLE_DATE,
    };
    use chrono::NaiveDate;
    use imessage_database::{
        message_types::{
            edited::{EditStatus, EditedMessage, EditedMessagePart},
            text_effects::text_effect::TextEffect,
        },
        rusqlite::{Connection, params},
        tables::{
            chat::Chat,
            messages::models::{AttachmentMeta, AttributedRange, BubbleComponent},
            table::{FITNESS_RECEIVER, ME},
        },
    };

    fn fake_chat() -> Chat {
        Chat {
            rowid: 0,
            chat_identifier: "Default".to_string(),
            service_name: Some(String::new()),
            display_name: None,
            is_filtered: None,
            is_blackholed: None,
            is_pending_review: None,
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
        assert_eq!(format_thread_header("Andrew"), "## Andrew\n\n".to_string());
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
        let config = Config::fake_app(isolated_options("can_create_timeline_exporter"));
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
        let config = Config::fake_app(isolated_options("format_timeline_message_basic_from_me"));
        let exporter = Timeline::new(&config).unwrap();

        let mut message = Config::fake_message_with_text("Hello world");
        // May 17, 2022 8:29:42 PM UTC -> 5:29:42 PM PT
        message.date = 674526582885055488;
        message.is_from_me = true;
        message.chat_id = Some(0);

        let actual = exporter.format_timeline_message(&message).unwrap();
        let expected = "**5:29:42 PM — Me**\n\nHello world\n\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn format_timeline_message_basic_from_them() {
        let mut config =
            Config::fake_app(isolated_options("format_timeline_message_basic_from_them"));
        config
            .participants
            .insert(999_999, Name::fake_name("Sample Contact"));
        config.real_participants.insert(999_999, 999_999);
        let exporter = Timeline::new(&config).unwrap();

        let mut message = Config::fake_message_with_text("Hello world");
        message.date = 674526582885055488;
        message.handle_id = Some(999_999);

        let actual = exporter.format_timeline_message(&message).unwrap();
        let expected = "**5:29:42 PM — Sample Contact**\n\nHello world\n\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn slice_text_for_bubble_returns_only_the_attribute_range() {
        let attrs = vec![AttributedRange::text(6, 11, vec![TextEffect::Default])];
        assert_eq!(slice_text_for_bubble("Hello world", &attrs), "world");
    }

    #[test]
    fn slice_text_for_bubble_dedupes_consecutive_identical_ranges() {
        // The typedstream parser sometimes emits multiple adjacent attribute
        // entries with the same byte range. We should render the slice once.
        let attrs = vec![
            AttributedRange::text(0, 5, vec![TextEffect::Default]),
            AttributedRange::text(0, 5, vec![TextEffect::Default]),
        ];
        assert_eq!(slice_text_for_bubble("Hello world", &attrs), "Hello");
    }

    #[test]
    fn multi_text_bubble_message_does_not_duplicate_text() {
        // Reproduces the bug where a message split into multiple Text bubbles
        // (e.g. mention + plain text) would render the entire `msg.text`
        // body once per Text bubble.
        let config = Config::fake_app(isolated_options(
            "multi_text_bubble_message_does_not_duplicate_text",
        ));
        let exporter = Timeline::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.text = Some("Hi Alice, how are you?".to_string());
        message.is_from_me = true;
        // Two text bubbles: "Hi Alice" + ", how are you?"
        message.components = vec![
            BubbleComponent::Run(vec![AttributedRange::text(0, 8, vec![TextEffect::Default])]),
            BubbleComponent::Run(vec![AttributedRange::text(
                8,
                22,
                vec![TextEffect::Default],
            )]),
        ];

        let actual = exporter.format_timeline_message(&message).unwrap();
        // Each bubble's slice appears once and on its own line.
        let expected = "**5:29:42 PM — Me**\n\nHi Alice\n\n, how are you?\n\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn format_announcement_renames_conversation() {
        let mut config =
            Config::fake_app(isolated_options("format_announcement_renames_conversation"));
        config.participants.insert(0, Name::fake_name(ME));
        let exporter = Timeline::new(&config).unwrap();

        let mut message = Config::fake_message();
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;
        message.item_type = 2;

        let actual = exporter.format_announcement(&message);
        let expected = "*5:29:42 PM — You renamed the conversation to Hello world.*\n\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn collect_message_groups_by_day_and_thread() {
        let mut config =
            Config::fake_app(isolated_options("collect_message_groups_by_day_and_thread"));
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
    fn thread_label_is_computed_once_per_chat() {
        // djf.5: chats with no name and no participants make upstream's
        // `Config::filename` print a "has no members" warning per call.
        let mut config =
            Config::fake_app(isolated_options("thread_label_is_computed_once_per_chat"));
        for (rowid, identifier) in [(1, "chat-one"), (2, "chat-two")] {
            let mut chat = fake_chat();
            chat.rowid = rowid;
            chat.chat_identifier = identifier.to_string();
            config.chatrooms.insert(rowid, chat);
            config.real_chatrooms.insert(rowid, rowid);
        }
        let mut exporter = Timeline::new(&config).unwrap();

        // Five messages in chat 1 over two days, three in chat 2.
        let one_day = 86_400_000_000_000;
        let messages = [
            (1, 0),
            (1, 1),
            (2, 2),
            (1, one_day),
            (2, one_day + 1),
            (1, one_day + 2),
            (2, one_day + 3),
            (1, one_day + 4),
        ];
        for (chat_id, offset) in messages {
            let mut msg = Config::fake_message();
            msg.date = SAMPLE_DATE + offset;
            msg.chat_id = Some(chat_id);
            exporter
                .collect_message(&msg, "block\n".to_string())
                .unwrap();
        }

        assert_eq!(exporter.thread_labels.computed, 2);
        assert_eq!(exporter.thread_labels.by_chat.len(), 2);
        // Cached headings are what an uncached call returns.
        for (rowid, expected) in [(1, "chat-one"), (2, "chat-two")] {
            assert_eq!(exporter.thread_labels.by_chat[&rowid], expected);
            assert_eq!(
                chat_display_label(&config, &config.chatrooms[&rowid]),
                expected
            );
        }
        // Both days carry both threads, each under its heading.
        assert_eq!(exporter.grouped.len(), 2);
        for bucket in exporter.grouped.values() {
            let mut names: Vec<&str> = bucket.values().map(|e| e.display_name.as_str()).collect();
            names.sort_unstable();
            assert_eq!(names, ["chat-one", "chat-two"]);
        }
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

        let early_block_pos = contents.find("EARLY-BLOCK").expect("missing EARLY-BLOCK");
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

    // MARK: App balloons (djf.2)
    /// Balloon bundle IDs as Messages stores them.
    const URL_BUNDLE: &str = "com.apple.messages.URLBalloonProvider";
    const POLL_BUNDLE: &str = "com.apple.messages.Polls";
    const OPENTABLE_BUNDLE: &str = "com.apple.messages.MSMessageExtensionBalloonPlugin:0000000000:com.opentable.OpenTable.OpenTableMessagesExtension";

    /// Rows added to the scratch database. Each carries one of upstream's
    /// fixture payloads from `imessage-database/test_data/`.
    const URL_ROW: i32 = 9001;
    const MUSIC_ROW: i32 = 9002;
    const POLL_ROW: i32 = 9003;
    const OPENTABLE_ROW: i32 = 9006;
    /// Message rowids joined to a sticker and to an ordinary attachment.
    const STICKER_ROW: i32 = 9007;
    const PHOTO_ROW: i32 = 9008;
    /// A parent message whose one reply is at `PARENT_ROW + 1`.
    const PARENT_ROW: i32 = 9020;
    const PARENT_GUID: &str = "timeline-test-parent";
    /// A rowid with no row in the scratch database, so it has no payload.
    const MISSING_ROW: i32 = 9999;
    const POLL_GUID: &str = "timeline-test-poll";

    /// The path of a copy of the fixture `test.db` with app balloon rows
    /// added, made once per test process. Payloads are read from the
    /// database by rowid, so a balloon cannot be rendered end to end without
    /// a real row. The fixture itself is never written to.
    fn seeded_db() -> &'static std::path::Path {
        static DB: OnceLock<std::path::PathBuf> = OnceLock::new();
        DB.get_or_init(|| {
            let test_data = std::env::current_dir()
                .unwrap()
                .parent()
                .unwrap()
                .join("imessage-database/test_data");
            let path = unique_test_dir("timeline-balloons-db").join("chat.db");
            std::fs::copy(test_data.join("db/test.db"), &path).unwrap();

            let rows = [
                SeedRow::balloon(
                    URL_ROW,
                    "timeline-test-url",
                    URL_BUNDLE,
                    "url_message/MetadataURL.plist",
                ),
                SeedRow::balloon(
                    MUSIC_ROW,
                    "timeline-test-music",
                    URL_BUNDLE,
                    "music_message/AppleMusic.plist",
                ),
                SeedRow::balloon(
                    POLL_ROW,
                    POLL_GUID,
                    POLL_BUNDLE,
                    "app_message/PollCreate.plist",
                ),
                SeedRow::vote(
                    9004,
                    "timeline-test-vote-1",
                    "app_message/PollVote.plist",
                    1,
                ),
                SeedRow::vote(
                    9005,
                    "timeline-test-vote-2",
                    "app_message/PollVote2.plist",
                    2,
                ),
                SeedRow::balloon(
                    OPENTABLE_ROW,
                    "timeline-test-opentable",
                    OPENTABLE_BUNDLE,
                    "app_message/OpenTableInvited.plist",
                ),
            ];

            let db = Connection::open(&path).unwrap();
            for row in rows {
                let payload = std::fs::read(test_data.join(row.payload)).unwrap();
                db.execute(
                    "INSERT INTO message (ROWID, guid, date, is_from_me, balloon_bundle_id, \
                     associated_message_guid, associated_message_type, payload_data) \
                     VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
                    params![
                        row.rowid,
                        row.guid,
                        row.date,
                        row.bundle_id,
                        row.associated_guid,
                        row.associated_type,
                        payload
                    ],
                )
                .unwrap();
            }

            // djf.8: a sticker and an ordinary attachment, each joined to a
            // message rowid that the attachment tests build by hand.
            for (attachment_rowid, message_rowid, name, is_sticker) in [
                (9101, STICKER_ROW, "outline.heic", true),
                (9102, PHOTO_ROW, "photo.jpg", false),
            ] {
                db.execute(
                    "INSERT INTO message (ROWID, guid, date, is_from_me) VALUES (?1, ?2, ?3, 1)",
                    params![
                        message_rowid,
                        format!("timeline-test-attachment-message-{message_rowid}"),
                        SAMPLE_DATE
                    ],
                )
                .unwrap();
                db.execute(
                    "INSERT INTO attachment (ROWID, guid, original_guid, transfer_name, is_sticker) \
                     VALUES (?1, ?2, ?2, ?3, ?4)",
                    params![
                        attachment_rowid,
                        format!("timeline-test-attachment-{attachment_rowid}"),
                        name,
                        is_sticker
                    ],
                )
                .unwrap();
                db.execute(
                    "INSERT INTO message_attachment_join (message_id, attachment_id) \
                     VALUES (?1, ?2)",
                    params![message_rowid, attachment_rowid],
                )
                .unwrap();
            }

            // djf.9: a parent message and a reply whose text reads as a heading.
            db.execute(
                "INSERT INTO message (ROWID, guid, date, is_from_me, text) \
                 VALUES (?1, ?2, ?3, 1, 'parent')",
                params![PARENT_ROW, PARENT_GUID, SAMPLE_DATE],
            )
            .unwrap();
            db.execute(
                "INSERT INTO message (ROWID, guid, date, is_from_me, text, thread_originator_guid) \
                 VALUES (?1, 'timeline-test-reply', ?2, 1, '# reply heading', ?3)",
                params![PARENT_ROW + 1, SAMPLE_DATE + 1, PARENT_GUID],
            )
            .unwrap();
            path
        })
    }

    /// One message row added to the scratch database.
    struct SeedRow {
        rowid: i32,
        guid: &'static str,
        bundle_id: &'static str,
        associated_guid: Option<&'static str>,
        associated_type: i32,
        /// Fixture path under `imessage-database/test_data/`.
        payload: &'static str,
        date: i64,
    }

    impl SeedRow {
        /// A message whose body is one app balloon.
        fn balloon(
            rowid: i32,
            guid: &'static str,
            bundle_id: &'static str,
            payload: &'static str,
        ) -> Self {
            Self {
                rowid,
                guid,
                bundle_id,
                associated_guid: None,
                associated_type: 0,
                payload,
                date: SAMPLE_DATE,
            }
        }

        /// A vote on the seeded poll, `order` nanoseconds after it.
        fn vote(rowid: i32, guid: &'static str, payload: &'static str, order: i64) -> Self {
            Self {
                rowid,
                guid,
                bundle_id: POLL_BUNDLE,
                associated_guid: Some(POLL_GUID),
                associated_type: 4000,
                payload,
                date: SAMPLE_DATE + order,
            }
        }
    }

    /// A timeline exporter config that reads the seeded database.
    fn seeded_config(test_name: &str) -> Config {
        let mut options = isolated_options(test_name);
        options.db_path = seeded_db().to_path_buf();
        Config::fake_app(options)
    }

    /// An app message from me at 5:29:42 PM PT whose body is one app balloon.
    fn app_message(rowid: i32, guid: &str, bundle_id: &str, text: Option<&str>) -> Message {
        let mut message = Config::fake_message();
        message.rowid = rowid;
        message.guid = guid.to_string();
        message.date = SAMPLE_DATE;
        message.is_from_me = true;
        message.balloon_bundle_id = Some(bundle_id.to_string());
        message.text = text.map(str::to_string);
        message.components = vec![BubbleComponent::App];
        message
    }

    #[test]
    fn url_preview_renders_link_title_and_summary() {
        let config = seeded_config("url_preview_renders_link_title_and_summary");
        let exporter = Timeline::new(&config).unwrap();
        let message = app_message(
            URL_ROW,
            "timeline-test-url",
            URL_BUNDLE,
            Some("https://chrissardegna.com"),
        );

        let actual = exporter.format_timeline_message(&message).unwrap();

        assert_eq!(
            actual,
            "**5:29:42 PM — Me**\n\n*Link*\n<https://chrissardegna.com>\nChristopher Sardegna\nSample page description\n\n"
        );
    }

    #[test]
    fn url_without_preview_payload_renders_its_text_as_the_link() {
        let config = seeded_config("url_without_preview_payload_renders_its_text_as_the_link");
        let exporter = Timeline::new(&config).unwrap();
        let message = app_message(
            MISSING_ROW,
            "timeline-test-no-preview",
            URL_BUNDLE,
            Some("https://example.com/no-preview"),
        );

        let actual = exporter.format_timeline_message(&message).unwrap();

        assert_eq!(
            actual,
            "**5:29:42 PM — Me**\n\n*Link*\n<https://example.com/no-preview>\n\n"
        );
    }

    #[test]
    fn apple_music_share_renders_track_album_artist_and_link() {
        let config = seeded_config("apple_music_share_renders_track_album_artist_and_link");
        let exporter = Timeline::new(&config).unwrap();
        let message = app_message(MUSIC_ROW, "timeline-test-music", URL_BUNDLE, None);

        let actual = exporter.format_timeline_message(&message).unwrap();

        assert_eq!(
            actual,
            "**5:29:42 PM — Me**\n\n*Music*\nПеснь 1\nПанихида\nБАТЮШКА\n<https://music.apple.com/us/album/%D0%BF%D0%B5%D1%81%D0%BD%D1%8C-1/1539641998?i=1539641999>\n\n"
        );
    }

    #[test]
    fn poll_renders_options_with_votes_and_voters() {
        let config = seeded_config("poll_renders_options_with_votes_and_voters");
        let exporter = Timeline::new(&config).unwrap();
        let message = app_message(POLL_ROW, POLL_GUID, POLL_BUNDLE, None);

        let actual = exporter.format_timeline_message(&message).unwrap();

        assert_eq!(
            actual,
            "**5:29:42 PM — Me**\n\n*Poll*\n- Option 1 (1)\n  - steven@applenet.com\n- Option 2 (2)\n  - +12223334444\n  - steven@applenet.com\n- Option 3 (0)\n- Option 4 (0)\n\n"
        );
    }

    #[test]
    fn generic_app_renders_its_name_and_fields() {
        let config = seeded_config("generic_app_renders_its_name_and_fields");
        let exporter = Timeline::new(&config).unwrap();
        let message = app_message(
            OPENTABLE_ROW,
            "timeline-test-opentable",
            OPENTABLE_BUNDLE,
            None,
        );

        let actual = exporter.format_timeline_message(&message).unwrap();

        assert_eq!(
            actual,
            "**5:29:42 PM — Me**\n\n*OpenTable*\nRusty Grill - Boise\nReservation Confirmed\nTable for 4 people\nSunday, October 17 at 7:45 PM\nYou're invited! Tap to accept.\n\n"
        );
    }

    #[test]
    fn unreadable_app_balloon_falls_back_to_the_old_output() {
        let config = seeded_config("unreadable_app_balloon_falls_back_to_the_old_output");
        let exporter = Timeline::new(&config).unwrap();

        // No payload row: upstream's dispatcher returns an error for each.
        let no_text = app_message(MISSING_ROW, "timeline-test-x", OPENTABLE_BUNDLE, None);
        let with_text = app_message(
            MISSING_ROW,
            "timeline-test-y",
            OPENTABLE_BUNDLE,
            Some("  hello  "),
        );
        let url_no_text = app_message(MISSING_ROW, "timeline-test-z", URL_BUNDLE, None);
        for message in [&no_text, &with_text, &url_no_text] {
            assert!(render_app_balloon(&config, message, &[]).is_err());
        }

        assert_eq!(
            exporter.format_timeline_message(&no_text).unwrap(),
            "**5:29:42 PM — Me**\n\n*[App message]*\n\n"
        );
        assert_eq!(
            exporter.format_timeline_message(&with_text).unwrap(),
            "**5:29:42 PM — Me**\n\n*hello*\n\n"
        );
        assert_eq!(
            exporter.format_timeline_message(&url_no_text).unwrap(),
            "**5:29:42 PM — Me**\n\n*[App message]*\n\n"
        );
    }

    #[test]
    fn unreadable_app_balloon_with_only_a_replacement_char_shows_the_placeholder() {
        // djf.10: an app message's text is often a lone U+FFFC, which is not
        // whitespace, so it used to print as an invisible `*￼*`.
        let config = seeded_config(
            "unreadable_app_balloon_with_only_a_replacement_char_shows_the_placeholder",
        );
        let exporter = Timeline::new(&config).unwrap();

        let only_char = app_message(
            MISSING_ROW,
            "timeline-test-fffc",
            OPENTABLE_BUNDLE,
            Some("\u{FFFC}"),
        );
        let char_and_spaces = app_message(
            MISSING_ROW,
            "timeline-test-fffc-spaces",
            OPENTABLE_BUNDLE,
            Some(" \u{FFFC}\u{FFFC} \n"),
        );
        // Real text next to the character is still shown, as before.
        let char_and_text = app_message(
            MISSING_ROW,
            "timeline-test-fffc-text",
            OPENTABLE_BUNDLE,
            Some("\u{FFFC}hello"),
        );
        for message in [&only_char, &char_and_spaces, &char_and_text] {
            assert!(render_app_balloon(&config, message, &[]).is_err());
        }

        assert_eq!(
            exporter.format_timeline_message(&only_char).unwrap(),
            "**5:29:42 PM — Me**\n\n*[App message]*\n\n"
        );
        assert_eq!(
            exporter.format_timeline_message(&char_and_spaces).unwrap(),
            "**5:29:42 PM — Me**\n\n*[App message]*\n\n"
        );
        assert_eq!(
            exporter.format_timeline_message(&char_and_text).unwrap(),
            "**5:29:42 PM — Me**\n\n*\u{FFFC}hello*\n\n"
        );
    }

    // MARK: Attachments (djf.8)
    /// A message from me at 5:29:42 PM PT whose body is one attachment.
    fn attachment_message(rowid: i32) -> Message {
        let mut message = Config::fake_message();
        message.rowid = rowid;
        message.date = SAMPLE_DATE;
        message.is_from_me = true;
        message.text = Some(OBJECT_REPLACEMENT_CHAR.to_string());
        message.num_attachments = 1;
        message.components = vec![BubbleComponent::Run(vec![AttributedRange::attachment(
            0,
            OBJECT_REPLACEMENT_CHAR.len_utf8(),
            AttachmentMeta::default(),
        )])];
        message
    }

    #[test]
    fn sticker_is_named_a_sticker() {
        let config = seeded_config("sticker_is_named_a_sticker");
        let exporter = Timeline::new(&config).unwrap();

        assert_eq!(
            exporter
                .format_timeline_message(&attachment_message(STICKER_ROW))
                .unwrap(),
            "**5:29:42 PM — Me**\n\n*[Sticker: outline.heic]*\n\n"
        );
        // An ordinary attachment keeps its old label.
        assert_eq!(
            exporter
                .format_timeline_message(&attachment_message(PHOTO_ROW))
                .unwrap(),
            "**5:29:42 PM — Me**\n\n*[Attachment: photo.jpg]*\n\n"
        );
    }

    // MARK: Heading escape (djf.9)
    #[test]
    fn escape_heading_lines_escapes_only_atx_headings() {
        for (input, expected) in [
            ("# hi", "\\# hi"),
            ("## hi", "\\## hi"),
            ("   # hi", "   \\# hi"),
            ("###### six", "\\###### six"),
            ("#", "\\#"),
            ("#\tafter a tab", "\\#\tafter a tab"),
            // Not headings: left alone.
            ("#blessed", "#blessed"),
            ("####### x", "####### x"),
            ("    # four spaces is code", "    # four spaces is code"),
            ("a # in the middle", "a # in the middle"),
            ("- # in a list item", "- # in a list item"),
            ("", ""),
            // Every line is checked, after `\n`, `\r\n` or `\r`.
            (
                "intro\n# one\n#two\n## three",
                "intro\n\\# one\n#two\n\\## three",
            ),
            ("intro\r\n# one\r# two", "intro\r\n\\# one\r\\# two"),
            ("# trailing\n", "\\# trailing\n"),
        ] {
            assert_eq!(escape_heading_lines(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn escape_heading_lines_is_idempotent_and_borrows_when_unchanged() {
        let once = escape_heading_lines("# a\n   ## b").into_owned();
        assert_eq!(escape_heading_lines(&once), once);
        assert!(matches!(escape_heading_lines("#blessed"), Cow::Borrowed(_)));
    }

    #[test]
    fn heading_shaped_message_lines_are_escaped() {
        let config = Config::fake_app(isolated_options("heading_shaped_message_lines_are_escaped"));
        let exporter = Timeline::new(&config).unwrap();

        let mut message =
            Config::fake_message_with_text("# hi\n## hi\n   # hi\n#blessed\n####### x");
        message.date = SAMPLE_DATE;
        message.is_from_me = true;
        message.subject = Some("Plans\n# not a heading".to_string());

        let actual = exporter.format_timeline_message(&message).unwrap();
        assert_eq!(
            actual,
            "**5:29:42 PM — Me**\n\n*Subject:* Plans\n\\# not a heading\n\n\\# hi\n\\## hi\n   \\# hi\n#blessed\n####### x\n\n"
        );
    }

    #[test]
    fn heading_shaped_reply_line_is_escaped_inside_the_blockquote() {
        let config = seeded_config("heading_shaped_reply_line_is_escaped_inside_the_blockquote");
        let exporter = Timeline::new(&config).unwrap();

        let mut parent = Config::fake_message_with_text("parent");
        parent.rowid = PARENT_ROW;
        parent.guid = PARENT_GUID.to_string();
        parent.date = SAMPLE_DATE;
        parent.is_from_me = true;
        parent.num_replies = 1;

        let actual = exporter.format_timeline_message(&parent).unwrap();
        assert_eq!(
            actual,
            "**5:29:42 PM — Me**\n\nparent\n\n> **5:29:42 PM — Me**\n>\n> \\# reply heading\n>\n\n"
        );
    }

    #[test]
    fn heading_shaped_line_in_app_fallback_text_is_escaped() {
        let config = seeded_config("heading_shaped_line_in_app_fallback_text_is_escaped");
        let exporter = Timeline::new(&config).unwrap();
        let message = app_message(
            MISSING_ROW,
            "timeline-test-heading-fallback",
            OPENTABLE_BUNDLE,
            Some("first\n# second"),
        );
        assert!(render_app_balloon(&config, &message, &[]).is_err());

        assert_eq!(
            exporter.format_timeline_message(&message).unwrap(),
            "**5:29:42 PM — Me**\n\n*first\n\\# second*\n\n"
        );
    }

    #[test]
    fn edited_part_is_marked_edited() {
        let config = Config::fake_app(isolated_options("edited_part_is_marked_edited"));
        let exporter = Timeline::new(&config).unwrap();

        let mut message = Config::fake_message_with_text("Final wording");
        message.date = SAMPLE_DATE;
        message.is_from_me = true;
        message.date_edited = SAMPLE_DATE + 60_000_000_000;
        message.edited_parts = Some(EditedMessage {
            parts: vec![EditedMessagePart {
                status: EditStatus::Edited,
                edit_history: vec![],
                unsend_failed: false,
            }],
            unsupported_handles: vec![],
        });

        let actual = exporter.format_timeline_message(&message).unwrap();
        assert_eq!(
            actual,
            "**5:29:42 PM — Me**\n\nFinal wording *(edited)*\n\n"
        );

        // A part that was not edited gets no marker.
        if let Some(parts) = message.edited_parts.as_mut() {
            parts.parts[0].status = EditStatus::Original;
        }
        let actual = exporter.format_timeline_message(&message).unwrap();
        assert_eq!(actual, "**5:29:42 PM — Me**\n\nFinal wording\n\n");
    }

    #[test]
    fn fitness_receiver_placeholder_reads_as_you() {
        let config = Config::fake_app(isolated_options(
            "fitness_receiver_placeholder_reads_as_you",
        ));
        let exporter = Timeline::new(&config).unwrap();

        let mut message =
            Config::fake_message_with_text(format!("{FITNESS_RECEIVER} closed all three rings"));
        message.date = SAMPLE_DATE;
        message.is_from_me = true;

        let actual = exporter.format_timeline_message(&message).unwrap();
        assert_eq!(
            actual,
            "**5:29:42 PM — Me**\n\nYou closed all three rings\n\n"
        );
    }

    fn tempdir_for_test(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("imessage_timeline_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build `Options` whose `export_path` is a tempdir unique to the test, so
    /// parallel tests don't race on a shared `/tmp/timeline.md`.
    fn isolated_options(test_name: &str) -> Options {
        let mut options = Options::fake_options(ExportType::Timeline);
        options.export_path = tempdir_for_test(test_name);
        options
    }
}
