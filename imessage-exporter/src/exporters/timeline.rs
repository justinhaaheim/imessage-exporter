/*!
 The Timeline exporter writes a single Markdown file that contains every
 message across every thread, grouped first by day and then by conversation
 thread. Within each thread on a given day, messages are listed in
 chronological order. Threads are not interleaved: each day shows full
 transcripts of every active thread, one after the other.
*/

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write as FmtWrite,
    fs::{File, remove_file, rename},
    io::{BufWriter, IsTerminal, Write, stderr},
    path::PathBuf,
};

use chrono::{DateTime, Datelike, NaiveDate};

use crate::{
    app::{
        error::RuntimeError,
        progress::ExportProgress,
        runtime::{Config, ORPHANED, UNKNOWN},
        zone::Zone,
    },
    exporters::{
        shared::{
            announcement::{YOU, resolve_announcement},
            balloon::rewrite_fitness_receiver,
            part::{AttachmentResolver, resolve_run},
        },
        timeline_balloons::render_app_balloon,
    },
};

use imessage_database::{
    error::table::TableError,
    message_types::variants::{Announcement, KeptMessage, Tapback, TapbackAction, Variant},
    rusqlite,
    tables::{
        attachment::Attachment,
        chat::Chat,
        chat_handle::Participant,
        messages::{
            Message,
            models::{AttributedRange, BubbleComponent, GroupAction},
        },
    },
};

/// File name (relative to the export path) of the unified timeline output.
pub(crate) const TIMELINE_FILENAME: &str = "timeline.md";

/// File name (relative to the export path) the timeline is written to
/// before it replaces [`TIMELINE_FILENAME`] (djf.12 F8).
pub(crate) const TIMELINE_PARTIAL_FILENAME: &str = "timeline.md.partial";

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

/// Separator between participant names in a thread heading, the same one
/// upstream's `Config::filename` puts between names in a file name.
const PARTICIPANT_SEPARATOR: &str = ", ";

/// Shown for an attachment range with no matching attachment row.
const ATTACHMENT_MISSING_LABEL: &str = "*[Attachment: missing]*";

/// Shown in place of a time when a message's date cannot be converted.
const UNKNOWN_TIME: &str = "(unknown time)";

/// Shown under a reply's header in its own day when the message it answers
/// cannot be found (djf.12 F2).
const REPLY_TO_UNKNOWN_PARENT: &str = "*(reply to an earlier message)*";

// MARK: Placement
/// Where a message block is shown (djf.12 F2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Placement {
    /// Under the message's own day. A reply shown here gets a line naming
    /// when the message it answers was sent.
    OwnDay,
    /// Inside the block of the message it replies to, which sits under the
    /// day `shown_on` (`None` when that message's date cannot be converted).
    Nested { shown_on: Option<NaiveDate> },
}

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
/// The heading comes from [`chat_display_label`], which, like upstream's
/// `Config::filename`, prints "Found error: message chat ID N has no
/// members!" each time it runs for a chat with no participants. The TXT and
/// HTML exporters call `Config::filename` once per chat, so the timeline
/// caches the heading and computes it once per chat too (djf.5). Keyed by
/// the chat row passed to [`chat_display_label`], so every heading is
/// exactly what an uncached call would return.
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
    /// The temporary file the unified timeline is written into. It replaces
    /// `timeline.md` only when the export succeeds (djf.12 F8).
    pub file: BufWriter<File>,
    /// Where `file` is: [`TIMELINE_PARTIAL_FILENAME`] in the export path.
    partial_path: PathBuf,
    /// Where the finished timeline goes: [`TIMELINE_FILENAME`] in the
    /// export path.
    final_path: PathBuf,
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
    /// Messages left out because they could not be read (djf.7). Each one is
    /// reported on stderr when it happens, and the total at the end.
    skipped_messages: u64,
    /// Progress bar model
    pb: ExportProgress,
}

// MARK: Exporter
impl<'a> Timeline<'a> {
    pub fn new(config: &'a Config) -> Result<Self, RuntimeError> {
        let final_path = config.options.export_path.join(TIMELINE_FILENAME);
        let partial_path = config.options.export_path.join(TIMELINE_PARTIAL_FILENAME);

        // djf.12 F8: write a temporary file next to timeline.md and rename
        // it over timeline.md only when the export succeeds, so a failed run
        // leaves a previous timeline.md as it was. A temporary file left by
        // a run that was killed is overwritten.
        let file = File::create(&partial_path)?;

        Ok(Timeline {
            config,
            file: BufWriter::new(file),
            partial_path,
            final_path,
            grouped: BTreeMap::new(),
            thread_labels: ThreadLabels::default(),
            skipped_messages: 0,
            pb: ExportProgress::new(config.options.show_progress && stderr().is_terminal()),
        })
    }

    /// Export every message into `timeline.md`. On failure, `timeline.md` is
    /// left as it was and the temporary file is removed (djf.12 F8).
    pub fn iter_messages(&mut self) -> Result<(), RuntimeError> {
        let result = self.write_timeline();
        if result.is_err()
            && let Err(why) = remove_file(&self.partial_path)
        {
            eprintln!(
                "Warning: could not remove the incomplete timeline {}: {why}",
                self.partial_path.display()
            );
        }
        result
    }

    /// Write the whole timeline into the temporary file, then move it over
    /// `timeline.md`.
    fn write_timeline(&mut self) -> Result<(), RuntimeError> {
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
                // djf.7: as upstream's shared driver does for TXT and HTML,
                // report a message that cannot be read and keep going, so
                // one bad row never ends the whole export.
                match self.format_timeline_message(&msg) {
                    Ok(block) => self.collect_message(&msg, block)?,
                    Err(why) => self.skip_message(&msg, &why),
                }
            }

            current_message += 1;
            if current_message % 99 == 0 {
                self.pb.set_position(current_message);
            }
        }

        self.flush_grouped()?;
        self.finish()?;
        self.pb.finish();

        if self.skipped_messages > 0 {
            eprintln!(
                "{} messages skipped due to formatting errors.",
                self.skipped_messages
            );
        }
        Ok(())
    }
}

// MARK: Grouping
impl<'a> Timeline<'a> {
    /// Leave `msg` out of the timeline because it cannot be read, and report
    /// it on stderr (djf.7). The total is reported at the end.
    fn skip_message(&mut self, msg: &Message, why: &dyn std::fmt::Display) {
        self.skipped_messages += 1;
        eprintln!(
            "Skipping message (rowid={}, guid={}): {}",
            msg.rowid, msg.guid, why
        );
    }

    /// Add a fully-formatted message block to the in-memory grouping, keyed
    /// by the message's local date and thread.
    fn collect_message(&mut self, msg: &Message, block: String) -> Result<(), RuntimeError> {
        // A message whose date cannot be converted has no day to sit under.
        // Skip it rather than failing the whole export, and report it like
        // any other message that cannot be read (djf.12 F7).
        let local = match msg.date() {
            Ok(d) => d.with_timezone(&self.config.options.timezone),
            Err(why) => {
                self.skip_message(msg, &why);
                return Ok(());
            }
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

    /// Flush the temporary file to disk and rename it over `timeline.md`
    /// (djf.12 F8). A rename within one directory replaces the old file in
    /// one step, so `timeline.md` is never half written.
    fn finish(&mut self) -> Result<(), RuntimeError> {
        self.file.flush().map_err(RuntimeError::DiskError)?;
        self.file
            .get_ref()
            .sync_all()
            .map_err(RuntimeError::DiskError)?;
        rename(&self.partial_path, &self.final_path).map_err(RuntimeError::DiskError)
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
            Err(_) => UNKNOWN_TIME.to_string(),
        }
    }

    /// The local date and time of `msg`, or `None` when its date cannot be
    /// converted.
    fn local_date_time(&self, msg: &Message) -> Option<DateTime<Zone>> {
        msg.date()
            .ok()
            .map(|d| d.with_timezone(&self.config.options.timezone))
    }

    /// The time shown in the header of a reply nested under the message it
    /// answers (djf.12 F2). The block sits under the day `shown_on`, so a
    /// reply sent that day shows only its time, as before, and a reply sent
    /// on another day shows its full date and time.
    fn format_nested_time(&self, msg: &Message, shown_on: Option<NaiveDate>) -> String {
        match self.local_date_time(msg) {
            Some(local) if shown_on == Some(local.date_naive()) => format_time(&local),
            Some(local) => format_date_time(&local),
            None => UNKNOWN_TIME.to_string(),
        }
    }

    /// The italic line under a reply's header in its own day (djf.12 F2),
    /// naming when the message it answers was sent, since that message and
    /// this reply's other copy may sit under another day.
    ///
    /// A parent that is not in the database, or whose date cannot be
    /// converted, gives `*(reply to an earlier message)*`. Any other database
    /// error is returned, so the message is reported and skipped like any
    /// other message that cannot be read (djf.7).
    fn format_reply_context(&self, parent_guid: &str) -> Result<String, TableError> {
        let parent = match Message::from_guid(
            parent_guid,
            self.config.data_source.db(),
            &self.config.data_source.capabilities,
        ) {
            Ok(parent) => Some(parent),
            Err(TableError::QueryError(rusqlite::Error::QueryReturnedNoRows)) => None,
            Err(why) => return Err(why),
        };
        Ok(
            match parent.as_ref().and_then(|p| self.local_date_time(p)) {
                Some(sent) => format!("*(reply to a message from {})*", format_date_time(&sent)),
                None => REPLY_TO_UNKNOWN_PARENT.to_string(),
            },
        )
    }

    /// Build the full Markdown block for a single (non-announcement) message
    /// shown under its own day: a header line, body, simple attachment
    /// placeholders, and any nested replies as Markdown blockquotes.
    fn format_timeline_message(&self, msg: &Message) -> Result<String, TableError> {
        self.format_message_block(msg, Placement::OwnDay)
    }

    /// [`Self::format_timeline_message`] for a message shown at `placement`.
    fn format_message_block(
        &self,
        msg: &Message,
        placement: Placement,
    ) -> Result<String, TableError> {
        let mut out = String::with_capacity(256);

        let time = match placement {
            Placement::OwnDay => self.format_time_of_day(msg),
            Placement::Nested { shown_on } => self.format_nested_time(msg, shown_on),
        };
        let sender = self
            .config
            .who(msg.handle_id, msg.is_from_me(), &msg.destination_caller_id);

        // Header line: bold time + em-dash + bold sender.
        let _ = writeln!(out, "**{time} — {sender}**");
        out.push('\n');

        // djf.12 F2: a reply is shown twice, nested under the message it
        // answers and under its own day. The nested copy is visibly a reply;
        // this copy says what it answers.
        if placement == Placement::OwnDay
            && let Some(parent_guid) = &msg.thread_originator_guid
        {
            let _ = writeln!(out, "{}", self.format_reply_context(parent_guid)?);
            out.push('\n');
        }

        if msg.is_deleted() {
            out.push_str("*(this message was deleted from the conversation)*\n\n");
        }

        if let Some(subject) = &msg.subject
            && !subject.is_empty()
        {
            let subject = escape_block_starts(subject);
            let _ = writeln!(out, "*Subject:* {subject}");
            out.push('\n');
        }

        let attachments = Attachment::from_message(
            self.config.data_source.db(),
            msg,
            &self.config.data_source.capabilities,
        )?;
        // djf.12 F3: pair each attachment range with its own attachment by
        // file-transfer GUID, as TXT and HTML do. Built once per message,
        // because GUID-less (legacy) ranges fall back to a positional cursor
        // that runs across the message's parts.
        let mut resolver = AttachmentResolver::new(&attachments);
        let mut wrote_body = false;

        // Render text, attachment and app components in order. Text and
        // attachments stay simpler than the TXT exporter (attachments are
        // named, not embedded); app balloons show the same fields TXT shows.
        for (part_index, component) in msg.components.iter().enumerate() {
            match component {
                BubbleComponent::Run(ranges) => {
                    // Render the run's text, then one label per attachment
                    // range in body order.
                    let text_ranges: Vec<AttributedRange> = ranges
                        .iter()
                        .filter(|r| r.attachment.is_none())
                        .cloned()
                        .collect();
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
                            // djf.9, djf.12 F1: no line of the text may read
                            // as a heading or open a fence or HTML block.
                            let trimmed = escape_block_starts(trimmed);
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

                    // djf.12 F3: one label per attachment range, so a bubble
                    // with several inline stickers or genmoji names each one.
                    for (_, index) in resolve_run(ranges, &mut resolver) {
                        let Some(index) = index else {
                            continue;
                        };
                        match attachments.get(index) {
                            Some(att) => {
                                let _ = writeln!(out, "{}", attachment_label(att));
                            }
                            None => {
                                let _ = writeln!(out, "{ATTACHMENT_MISSING_LABEL}");
                            }
                        }
                        out.push('\n');
                        wrote_body = true;
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
                let trimmed = escape_block_starts(trimmed);
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
        // Every nested copy sits under the day of the outermost message.
        let shown_on = match placement {
            Placement::OwnDay => self.local_date_time(msg).map(|d| d.date_naive()),
            Placement::Nested { shown_on } => shown_on,
        };
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
                    let nested =
                        self.format_message_block(reply, Placement::Nested { shown_on })?;
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

/// Format a `DateTime` in the export's timezone as "Fri, Feb 7, 2025 1:25:24
/// PM", for a time shown away from its own day heading (djf.12 F2). chrono's
/// `%a` and `%b` are always the English abbreviations.
pub(crate) fn format_date_time(date: &DateTime<Zone>) -> String {
    date.format("%a, %b %-d, %Y %-l:%M:%S %p").to_string()
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

/// The italic label naming one attachment, e.g. `*[Attachment: photo.jpg]*`.
/// A sticker is named a sticker, as TXT does (djf.8).
fn attachment_label(att: &Attachment) -> String {
    let (kind, fallback) = if att.is_sticker {
        ("Sticker", "sticker")
    } else {
        ("Attachment", "attachment")
    };
    let label = att
        .filename()
        .or(att.transfer_name.as_deref())
        .unwrap_or(fallback);
    format!("*[{kind}: {label}]*")
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
    // djf.9, djf.12 F1: a later line of a multi-line text could read as a
    // heading or open a fence or HTML block.
    format!("*{}*", escape_block_starts(label))
}

/// Escape every line of `text` that CommonMark would read as the start of a
/// heading, a code fence or an HTML block, so message text never looks like
/// a day or thread heading and never hides the messages after it (djf.9,
/// djf.12 F1).
///
/// A backslash goes before the line's first marker character, which Markdown
/// renders as that plain character. A line is escaped when, after up to three
/// spaces, it is:
///
/// - an ATX heading: one to six `#`, then a space, a tab or the end of the
///   line. Hashtags such as `#blessed` and seven or more `#` are not headings;
/// - a code fence: three or more `` ` `` with no `` ` `` later on the line,
///   or three or more `~`. Left open, a fence turns the rest of the file into
///   code;
/// - an HTML block of CommonMark types 1 to 5, which runs to its own end
///   marker rather than to the next blank line, so it too can swallow the
///   rest of the file: `<pre`, `<script`, `<style` or `<textarea` (any case,
///   then a space, a tab, `>` or the end of the line), `<!--`, `<?`, `<!` and
///   a letter, or `<![CDATA[`;
/// - a setext heading underline (djf.11): only `=` or only `-`, then only
///   spaces or tabs, directly under a line that is not blank. A line of `-`
///   under a blank line is a horizontal rule, not a heading, and is left
///   alone.
///
/// The first line of `text` counts as following a blank line, as message
/// text does in the timeline; [`escape_block_starts_after`] takes the
/// other case. Lines indented four or more spaces, or by a tab, are code and
/// are left alone. Lines end at `\r\n`, `\n` or `\r`, as in CommonMark.
/// Escaping an escaped text changes nothing.
pub(crate) fn escape_block_starts(text: &str) -> Cow<'_, str> {
    escape_block_starts_after(text, false)
}

/// [`escape_block_starts`], where `follows_text` says whether the line just
/// before `text` (outside it) is a line that is not blank, which makes a
/// first line of only `=` or `-` a setext heading underline.
pub(crate) fn escape_block_starts_after(text: &str, follows_text: bool) -> Cow<'_, str> {
    let mut escaped: Option<String> = None;
    let mut copied = 0;
    let mut line_start = 0;
    let mut previous_blank = !follows_text;
    loop {
        let rest = &text[line_start..];
        let (line_len, ending_len) = match rest.find(['\n', '\r']) {
            Some(end) if rest[end..].starts_with("\r\n") => (end, 2),
            Some(end) => (end, 1),
            None => (rest.len(), 0),
        };
        let line = &rest[..line_len];
        if let Some(indent) = block_start_indent(line, previous_blank) {
            let marker_at = line_start + indent;
            let out = escaped.get_or_insert_with(|| String::with_capacity(text.len() + 8));
            out.push_str(&text[copied..marker_at]);
            out.push('\\');
            copied = marker_at;
        }
        previous_blank = is_blank_line(line);
        if ending_len == 0 {
            break;
        }
        line_start += line_len + ending_len;
    }
    match escaped {
        Some(mut out) => {
            out.push_str(&text[copied..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(text),
    }
}

/// `true` when `line` (without its line ending) is empty or only spaces and
/// tabs, which is what CommonMark calls a blank line.
pub(crate) fn is_blank_line(line: &str) -> bool {
    line.bytes().all(|b| b == b' ' || b == b'\t')
}

/// The number of leading spaces when `line` (without its line ending) starts
/// one of the blocks [`escape_block_starts`] escapes, else `None`.
/// `previous_blank` says whether the line before it is blank.
fn block_start_indent(line: &str, previous_blank: bool) -> Option<usize> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 {
        return None;
    }
    let marker = &line.as_bytes()[indent..];
    let starts_block = is_atx_heading(marker)
        || is_code_fence(marker)
        || is_html_block(marker)
        || (!previous_blank && is_setext_underline(marker));
    starts_block.then_some(indent)
}

/// One to six `#`, then a space, a tab or the end of the line.
fn is_atx_heading(marker: &[u8]) -> bool {
    let hashes = marker.iter().take_while(|&&b| b == b'#').count();
    (1..=6).contains(&hashes) && matches!(marker.get(hashes), None | Some(b' ' | b'\t'))
}

/// Three or more `` ` `` with no `` ` `` after them, or three or more `~`.
fn is_code_fence(marker: &[u8]) -> bool {
    let Some(&fence) = marker.first().filter(|&&b| b == b'`' || b == b'~') else {
        return false;
    };
    let run = marker.iter().take_while(|&&b| b == fence).count();
    run >= 3 && (fence == b'~' || !marker[run..].contains(&b'`'))
}

/// The start of an HTML block of CommonMark types 1 to 5.
fn is_html_block(marker: &[u8]) -> bool {
    let Some(rest) = marker.strip_prefix(b"<") else {
        return false;
    };
    if rest.starts_with(b"!--") || rest.starts_with(b"?") || rest.starts_with(b"![CDATA[") {
        return true;
    }
    if rest.first() == Some(&b'!') && rest.get(1).is_some_and(u8::is_ascii_alphabetic) {
        return true;
    }
    [&b"pre"[..], b"script", b"style", b"textarea"]
        .iter()
        .any(|tag| {
            rest.get(..tag.len())
                .is_some_and(|name| name.eq_ignore_ascii_case(tag))
                && matches!(rest.get(tag.len()), None | Some(b' ' | b'\t' | b'>'))
        })
}

/// Only `=` or only `-`, then only spaces or tabs.
fn is_setext_underline(marker: &[u8]) -> bool {
    let Some(&underline) = marker.first().filter(|&&b| b == b'=' || b == b'-') else {
        return false;
    };
    let run = marker.iter().take_while(|&&b| b == underline).count();
    marker[run..].iter().all(|&b| b == b' ' || b == b'\t')
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

/// Compute the chat label used as the H2 heading for a thread (djf.12 F10).
///
/// The heading names a conversation the way upstream's `Config::filename`
/// names its file, but whole. It is built from the conversation's
/// representative chat row, which every row of a merged conversation is
/// named after:
///
/// - its display name, then ` - ` and the deduplicated chat ID, as in the
///   file name, so two groups with the same name stay apart;
/// - else its participants' names, each person once, joined with `, `;
/// - else its chat identifier, after upstream's "has no members" warning.
///
/// Unlike a file name, the heading is never shortened to fit the export path
/// and keeps characters such as `/` and `:`, so it does not depend on `-o`.
/// A line break in a name becomes a space, because a heading is one line.
/// Headings are cached per chat (djf.5), so the warning prints once per
/// chat, as `Config::filename` prints it once per chat for TXT and HTML.
pub(crate) fn chat_display_label(config: &Config, chatroom: &Chat) -> String {
    let chatroom = config
        .chat_representatives
        .get(&chatroom.rowid)
        .and_then(|representative| config.chatrooms.get(representative))
        .unwrap_or(chatroom);

    let name = match chatroom.display_name() {
        Some(name) => format!(
            "{name} - {}",
            config
                .real_chatrooms
                .get(&chatroom.rowid)
                .unwrap_or(&chatroom.rowid)
        ),
        None => match config.chatroom_participants.get(&chatroom.rowid) {
            Some(participants) => participant_names(config, participants),
            None => {
                eprintln!(
                    "Found error: message chat ID {} has no members!",
                    chatroom.rowid
                );
                chatroom.chat_identifier.clone()
            }
        },
    };
    if name.is_empty() {
        if !chatroom.chat_identifier.is_empty() {
            return one_line(&chatroom.chat_identifier);
        }
        return UNKNOWN.to_string();
    }
    // Fix the legacy ORPHANED constant leaking through if somehow used.
    if name == ORPHANED {
        return ORPHANED_THREAD_LABEL.to_string();
    }
    one_line(&name)
}

/// The names of a chat's participants, each person once (several handles of
/// one contact count as one person), joined with `, `, in the order
/// upstream's `Config::filename` uses. A handle with no person shows as
/// `Unknown`.
fn participant_names(config: &Config, participants: &BTreeSet<i32>) -> String {
    let people: BTreeSet<Participant> = participants
        .iter()
        .map(|handle_id| match config.real_participants.get(handle_id) {
            Some(group) => Participant::Group(*group),
            None => Participant::Handle(*handle_id),
        })
        .collect();
    people
        .iter()
        .map(|person| match person {
            Participant::Group(group) => config
                .participants
                .get(group)
                .map_or(UNKNOWN, |name| name.details.as_str()),
            Participant::Handle(_) => UNKNOWN,
        })
        .collect::<Vec<&str>>()
        .join(PARTICIPANT_SEPARATOR)
}

/// `text` with each line break (`\r\n`, `\n` or `\r`) made a single space.
fn one_line(text: &str) -> String {
    if !text.contains(['\n', '\r']) {
        return text.to_string();
    }
    text.split(['\n', '\r'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<&str>>()
        .join(" ")
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

    // MARK: Thread headings (djf.12 F10)
    /// A timeline config whose export path is `export_path`.
    fn config_exporting_to(export_path: &str) -> Config {
        let mut options = Options::fake_options(ExportType::Timeline);
        options.export_path = std::path::PathBuf::from(export_path);
        Config::fake_app(options)
    }

    /// An export path long enough that upstream's file names are shortened.
    fn long_export_path() -> String {
        format!("/tmp/{}", "x".repeat(200))
    }

    #[test]
    fn heading_keeps_characters_a_file_name_cannot_hold() {
        let config = config_exporting_to("/tmp");
        let mut chat = fake_chat();
        chat.display_name = Some("Plans: A/B".to_string());

        // The file name replaces `:` and `/`; the heading keeps them.
        assert_eq!(config.filename(&chat), "Plans_ A_B - 0.md");
        assert_eq!(chat_display_label(&config, &chat), "Plans: A/B - 0");
    }

    #[test]
    fn heading_from_a_display_name_does_not_depend_on_the_export_path() {
        let name = "A group name long enough to be cut short in a file name".repeat(2);
        let mut chat = fake_chat();
        chat.display_name = Some(name.clone());
        let short = config_exporting_to("/tmp");
        let long = config_exporting_to(&long_export_path());

        // The file name is shortened to fit the long path.
        assert!(!long.filename(&chat).starts_with(&name));
        for config in [&short, &long] {
            assert_eq!(chat_display_label(config, &chat), format!("{name} - 0"));
        }
    }

    #[test]
    fn heading_from_participants_names_everyone_whatever_the_export_path() {
        let names = [
            "Participant with a fairly long name number one",
            "Participant with a fairly long name number two",
            "Participant with a fairly long name number three",
        ];
        let mut chat = fake_chat();
        chat.rowid = 5;
        let mut configs = [
            config_exporting_to("/tmp"),
            config_exporting_to(&long_export_path()),
        ];
        for config in &mut configs {
            config
                .chatroom_participants
                .insert(5, BTreeSet::from([11, 12, 13, 14]));
            for (handle, name) in (11..).zip(names) {
                config.real_participants.insert(handle, handle);
                config.participants.insert(handle, Name::fake_name(name));
            }
            // A second handle of the first person counts once.
            config.real_participants.insert(14, 11);
        }

        // The file name drops names to fit the long path.
        assert!(!configs[1].filename(&chat).contains(names[1]));
        for config in &configs {
            assert_eq!(chat_display_label(config, &chat), names.join(", "));
        }
    }

    #[test]
    fn heading_of_a_merged_conversation_uses_its_representative_row() {
        let mut config = config_exporting_to("/tmp");
        let mut named = fake_chat();
        named.rowid = 7;
        named.display_name = Some("Family".to_string());
        config.chatrooms.insert(7, named);
        config.chat_representatives.insert(6, 7);
        config.real_chatrooms.insert(6, 6);
        config.real_chatrooms.insert(7, 6);
        let mut unnamed = fake_chat();
        unnamed.rowid = 6;

        assert_eq!(config.filename(&unnamed), "Family - 6.md");
        assert_eq!(chat_display_label(&config, &unnamed), "Family - 6");
    }

    #[test]
    fn heading_puts_a_name_on_one_line() {
        let config = config_exporting_to("/tmp");
        let mut chat = fake_chat();
        chat.display_name = Some("Line one\r\nline two\nline three".to_string());

        assert_eq!(
            chat_display_label(&config, &chat),
            "Line one line two line three - 0"
        );
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
    fn message_whose_date_cannot_be_converted_is_skipped_and_counted() {
        // djf.12 F7: such a message used to vanish without a word.
        let mut config = Config::fake_app(isolated_options(
            "message_whose_date_cannot_be_converted_is_skipped_and_counted",
        ));
        config.chatrooms.insert(0, fake_chat());
        config.real_chatrooms.insert(0, 0);
        let mut exporter = Timeline::new(&config).unwrap();

        let mut bad_date = Config::fake_message_with_text("cannot be placed");
        bad_date.date = -9_000_000_000_000_000_000;
        bad_date.chat_id = Some(0);
        assert!(bad_date.date().is_err());
        let mut good_date = Config::fake_message_with_text("placed");
        good_date.date = SAMPLE_DATE;
        good_date.chat_id = Some(0);

        exporter
            .collect_message(&bad_date, "bad\n".to_string())
            .unwrap();
        exporter
            .collect_message(&good_date, "good\n".to_string())
            .unwrap();

        assert_eq!(exporter.skipped_messages, 1);
        let blocks: Vec<&String> = exporter
            .grouped
            .values()
            .flat_map(|bucket| bucket.values())
            .flat_map(|entry| &entry.messages)
            .collect();
        assert_eq!(blocks, ["good\n"]);
    }

    #[test]
    fn thread_label_is_computed_once_per_chat() {
        // djf.5: chats with no name and no participants make
        // `chat_display_label` print a "has no members" warning per call, as
        // upstream's `Config::filename` does.
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
        exporter.finish().unwrap();
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
        exporter.finish().unwrap();
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
        exporter.finish().unwrap();
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
    /// A parent message whose one reply, at `NEXT_DAY_PARENT_ROW + 1`, was
    /// sent the next day.
    const NEXT_DAY_PARENT_ROW: i32 = 9022;
    const NEXT_DAY_PARENT_GUID: &str = "timeline-test-parent-next-day";
    const NEXT_DAY_REPLY_GUID: &str = "timeline-test-reply-next-day";
    const ONE_DAY: i64 = 86_400_000_000_000;
    /// A message whose body is upstream's typedstream fixture
    /// `FormattedInlineStickers`: one bubble, two inline stickers.
    const INLINE_STICKERS_ROW: i32 = 9030;
    const INLINE_STICKERS_GUID: &str = "timeline-test-inline-stickers";
    /// The two stickers' file-transfer GUIDs, as the fixture body names them.
    const FIRST_STICKER_GUID: &str = "1E50CD07-D8F7-4CF1-A662-B23262B9D492";
    const SECOND_STICKER_GUID: &str = "49769DA1-FF04-4381-8982-B4810F0EB971";
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

            // djf.12 F2: a parent message and a reply sent the next day.
            db.execute(
                "INSERT INTO message (ROWID, guid, date, is_from_me, text) \
                 VALUES (?1, ?2, ?3, 1, 'parent')",
                params![NEXT_DAY_PARENT_ROW, NEXT_DAY_PARENT_GUID, SAMPLE_DATE],
            )
            .unwrap();
            db.execute(
                "INSERT INTO message (ROWID, guid, date, is_from_me, text, thread_originator_guid) \
                 VALUES (?1, ?2, ?3, 1, 'reply sent the next day', ?4)",
                params![
                    NEXT_DAY_PARENT_ROW + 1,
                    NEXT_DAY_REPLY_GUID,
                    SAMPLE_DATE + ONE_DAY,
                    NEXT_DAY_PARENT_GUID
                ],
            )
            .unwrap();

            // djf.12 F3: a bubble with two inline stickers. The join rows
            // list the second sticker first, so pairing by position would
            // name the stickers in the wrong order.
            let body = std::fs::read(test_data.join("typedstream/FormattedInlineStickers")).unwrap();
            db.execute(
                "INSERT INTO message (ROWID, guid, date, is_from_me, attributedBody) \
                 VALUES (?1, ?2, ?3, 1, ?4)",
                params![INLINE_STICKERS_ROW, INLINE_STICKERS_GUID, SAMPLE_DATE, body],
            )
            .unwrap();
            for (attachment_rowid, guid, name) in [
                (9103, SECOND_STICKER_GUID, "second-sticker.heic"),
                (9104, FIRST_STICKER_GUID, "first-sticker.heic"),
            ] {
                db.execute(
                    "INSERT INTO attachment (ROWID, guid, original_guid, transfer_name, is_sticker) \
                     VALUES (?1, ?2, ?2, ?3, 1)",
                    params![attachment_rowid, guid, name],
                )
                .unwrap();
                db.execute(
                    "INSERT INTO message_attachment_join (message_id, attachment_id) \
                     VALUES (?1, ?2)",
                    params![INLINE_STICKERS_ROW, attachment_rowid],
                )
                .unwrap();
            }
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

    #[test]
    fn every_inline_sticker_in_a_bubble_is_named_by_its_guid() {
        // djf.12 F3: the timeline named at most one attachment per bubble,
        // and paired attachments with bubbles by position.
        let config = seeded_config("every_inline_sticker_in_a_bubble_is_named_by_its_guid");
        let exporter = Timeline::new(&config).unwrap();
        let message = Message::from_guid(
            INLINE_STICKERS_GUID,
            config.data_source.db(),
            &config.data_source.capabilities,
        )
        .unwrap();

        // The fixture body: two inline sticker ranges, by GUID, in this order.
        let BubbleComponent::Run(ranges) = &message.components[0] else {
            panic!("expected one run, got {:?}", message.components);
        };
        let range_guids: Vec<&str> = ranges
            .iter()
            .filter_map(|r| r.attachment.as_ref()?.guid.as_deref())
            .collect();
        assert_eq!(range_guids, [FIRST_STICKER_GUID, SECOND_STICKER_GUID]);
        // The database returns the stickers in the other order.
        let attachments = Attachment::from_message(
            config.data_source.db(),
            &message,
            &config.data_source.capabilities,
        )
        .unwrap();
        let row_guids: Vec<&str> = attachments
            .iter()
            .filter_map(|a| a.guid.as_deref())
            .collect();
        assert_eq!(row_guids, [SECOND_STICKER_GUID, FIRST_STICKER_GUID]);

        assert_eq!(
            exporter.format_timeline_message(&message).unwrap(),
            "**5:29:42 PM — Me**\n\n🫪🙏\n\n*[Sticker: first-sticker.heic]*\n\n*[Sticker: second-sticker.heic]*\n\n"
        );
    }

    #[test]
    fn attachment_range_with_no_attachment_row_is_named_missing() {
        let config = seeded_config("attachment_range_with_no_attachment_row_is_named_missing");
        let exporter = Timeline::new(&config).unwrap();
        // Two attachment ranges, but the message has no attachment rows.
        let mut message = attachment_message(MISSING_ROW);
        message.text = Some("\u{FFFC}\u{FFFC}".to_string());
        message.components = vec![BubbleComponent::Run(vec![
            AttributedRange::attachment(0, 3, AttachmentMeta::default()),
            AttributedRange::attachment(3, 6, AttachmentMeta::default()),
        ])];

        assert_eq!(
            exporter.format_timeline_message(&message).unwrap(),
            "**5:29:42 PM — Me**\n\n*[Attachment: missing]*\n\n*[Attachment: missing]*\n\n"
        );
    }

    // MARK: Unreadable messages (djf.7)
    const UNREADABLE_ROW: i32 = 9100;
    const UNREADABLE_TEXT: &str = "this message cannot be read";

    /// A copy of the fixture `test.db` plus one message that cannot be read:
    /// its attachment join row points at an attachment that does not exist,
    /// as in a damaged database, so loading its attachments fails. It is the
    /// earliest message, so every fixture message is exported after it.
    fn db_with_unreadable_message() -> std::path::PathBuf {
        let test_data = std::env::current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data");
        let path = unique_test_dir("timeline-unreadable-db").join("chat.db");
        std::fs::copy(test_data.join("db/test.db"), &path).unwrap();

        let db = Connection::open(&path).unwrap();
        // The dangling join row is the damage, so allow it.
        db.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        db.execute(
            "INSERT INTO message (ROWID, guid, date, is_from_me, text) \
             VALUES (?1, 'timeline-test-unreadable', ?2, 1, ?3)",
            params![UNREADABLE_ROW, SAMPLE_DATE, UNREADABLE_TEXT],
        )
        .unwrap();
        db.execute(
            "INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (?1, 999999)",
            params![UNREADABLE_ROW],
        )
        .unwrap();
        path
    }

    #[test]
    fn unreadable_message_is_skipped_and_the_export_finishes() {
        let mut options = isolated_options("unreadable_message_is_skipped_and_the_export_finishes");
        options.db_path = db_with_unreadable_message();
        let export_path = options.export_path.clone();
        let config = Config::fake_app(options);

        // The seeded message really cannot be read.
        let mut unreadable = Config::fake_message();
        unreadable.rowid = UNREADABLE_ROW;
        unreadable.num_attachments = 1;
        assert!(
            Attachment::from_message(
                config.data_source.db(),
                &unreadable,
                &config.data_source.capabilities
            )
            .is_err()
        );

        let mut exporter = Timeline::new(&config).unwrap();
        exporter.iter_messages().unwrap();
        assert_eq!(exporter.skipped_messages, 1);
        drop(exporter);

        let contents = std::fs::read_to_string(export_path.join(TIMELINE_FILENAME)).unwrap();
        assert!(!contents.contains(UNREADABLE_TEXT), "got:\n{contents}");
        // All three fixture messages are still exported.
        for expected in [
            "*Link*",
            "eat as quick as possible",
            "Oh it translated what I sent!",
        ] {
            assert!(
                contents.contains(expected),
                "missing {expected:?} in:\n{contents}"
            );
        }
        let message_headers = contents.lines().filter(|l| l.starts_with("**")).count();
        assert_eq!(message_headers, 3, "got:\n{contents}");
    }

    // MARK: Failed exports (djf.12 F8)
    /// A copy of the fixture `test.db` plus one message row that cannot be
    /// read at all: its date is text, so reading the row fails and the
    /// export stops. Text sorts after numbers, so it is read last, after the
    /// timeline file has been written to.
    fn db_with_unparseable_row() -> std::path::PathBuf {
        let test_data = std::env::current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data");
        let path = unique_test_dir("timeline-unparseable-db").join("chat.db");
        std::fs::copy(test_data.join("db/test.db"), &path).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute(
            "INSERT INTO message (ROWID, guid, date, is_from_me, text) \
             VALUES (9200, 'timeline-test-unparseable', 'not a date', 1, 'x')",
            [],
        )
        .unwrap();
        path
    }

    #[test]
    fn failed_export_leaves_the_previous_timeline_unchanged() {
        let mut options = isolated_options("failed_export_leaves_the_previous_timeline_unchanged");
        options.db_path = db_with_unparseable_row();
        let export_path = options.export_path.clone();
        let previous = "# Monday, January 5, 2026\n\n## An earlier export\n\nkept as is\n";
        std::fs::write(export_path.join(TIMELINE_FILENAME), previous).unwrap();
        let config = Config::fake_app(options);

        let mut exporter = Timeline::new(&config).unwrap();
        // The export stops at the unparseable row, after writing has begun.
        let error = exporter.iter_messages().unwrap_err();
        assert!(
            matches!(error, RuntimeError::DatabaseError(_)) && error.to_string().contains("date"),
            "got: {error}"
        );
        drop(exporter);

        assert_eq!(
            std::fs::read_to_string(export_path.join(TIMELINE_FILENAME)).unwrap(),
            previous
        );
        assert!(!export_path.join(TIMELINE_PARTIAL_FILENAME).exists());
    }

    #[test]
    fn successful_export_replaces_the_previous_timeline() {
        let mut options = isolated_options("successful_export_replaces_the_previous_timeline");
        options.db_path = db_with_unreadable_message();
        let export_path = options.export_path.clone();
        std::fs::write(export_path.join(TIMELINE_FILENAME), "an earlier export\n").unwrap();
        let config = Config::fake_app(options);

        let mut exporter = Timeline::new(&config).unwrap();
        exporter.iter_messages().unwrap();
        drop(exporter);

        let contents = std::fs::read_to_string(export_path.join(TIMELINE_FILENAME)).unwrap();
        assert!(contents.starts_with("# "), "got:\n{contents}");
        assert!(!contents.contains("an earlier export"), "got:\n{contents}");
        assert!(!export_path.join(TIMELINE_PARTIAL_FILENAME).exists());
    }

    // MARK: Block-start escape (djf.9, djf.12 F1)
    #[test]
    fn escape_block_starts_escapes_atx_headings() {
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
            assert_eq!(escape_block_starts(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn escape_block_starts_escapes_code_fences() {
        for (input, expected) in [
            ("```", "\\```"),
            ("```rust", "\\```rust"),
            ("````", "\\````"),
            ("~~~", "\\~~~"),
            ("~~~ info with ` ticks", "\\~~~ info with ` ticks"),
            ("   ```", "   \\```"),
            (
                "here is the start of some code:\n```\nlet x = 1;",
                "here is the start of some code:\n\\```\nlet x = 1;",
            ),
            // Not fences: left alone.
            ("``", "``"),
            ("~~", "~~"),
            ("```inline``` code", "```inline``` code"),
            ("    ```", "    ```"),
            ("\t```", "\t```"),
            ("a ``` in the middle", "a ``` in the middle"),
        ] {
            assert_eq!(escape_block_starts(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn escape_block_starts_escapes_html_blocks_that_run_to_an_end_marker() {
        for (input, expected) in [
            // Types 1 to 5.
            ("<pre", "\\<pre"),
            ("<PRE>", "\\<PRE>"),
            ("<script src=x>", "\\<script src=x>"),
            ("<style\tx", "\\<style\tx"),
            ("<textarea>", "\\<textarea>"),
            ("<!-- note", "\\<!-- note"),
            ("<?php echo 1;", "\\<?php echo 1;"),
            ("<!DOCTYPE html>", "\\<!DOCTYPE html>"),
            ("<![CDATA[x", "\\<![CDATA[x"),
            ("  <!-- note", "  \\<!-- note"),
            ("intro\n<!-- note", "intro\n\\<!-- note"),
            // Other HTML ends at the next blank line, so it is left alone.
            ("<div>", "<div>"),
            ("<prefix", "<prefix"),
            ("<a href=\"x\">", "<a href=\"x\">"),
            ("<!-", "<!-"),
            ("<!1", "<!1"),
            ("    <!-- code", "    <!-- code"),
            ("a <!-- in the middle", "a <!-- in the middle"),
        ] {
            assert_eq!(escape_block_starts(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn escape_block_starts_escapes_setext_underlines_under_text() {
        // djf.11: a line of only `=` or `-` under a line of text makes that
        // line a heading.
        for (input, expected) in [
            ("Total\n-----", "Total\n\\-----"),
            ("Total\n===", "Total\n\\==="),
            ("Results\n===\ndone", "Results\n\\===\ndone"),
            ("Total\n-", "Total\n\\-"),
            ("Total\n   ===  ", "Total\n   \\===  "),
            ("Total\r\n===", "Total\r\n\\==="),
            ("a\n===\n===", "a\n\\===\n\\==="),
            // Under a blank line or at the start: a rule or text, left alone.
            ("Total\n\n-----", "Total\n\n-----"),
            ("Total\n  \t\n===", "Total\n  \t\n==="),
            ("---", "---"),
            ("===\nmore", "===\nmore"),
            // Not underlines: left alone.
            ("Total\n= =", "Total\n= ="),
            ("Total\n=-=", "Total\n=-="),
            ("Total\n    ===", "Total\n    ==="),
            ("Total\n--- x", "Total\n--- x"),
        ] {
            assert_eq!(escape_block_starts(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn escape_block_starts_after_text_escapes_a_first_underline_line() {
        assert_eq!(escape_block_starts_after("===", true), "\\===");
        assert_eq!(escape_block_starts_after("---\nx", true), "\\---\nx");
        assert_eq!(escape_block_starts_after("===", false), "===");
    }

    #[test]
    fn escape_block_starts_is_idempotent_and_borrows_when_unchanged() {
        let once = escape_block_starts("# a\n   ## b\n```\n<!-- c\nd\n===").into_owned();
        assert_eq!(once, "\\# a\n   \\## b\n\\```\n\\<!-- c\nd\n\\===");
        assert_eq!(escape_block_starts(&once), once);
        assert!(matches!(escape_block_starts("#blessed"), Cow::Borrowed(_)));
    }

    #[test]
    fn block_starting_message_lines_are_escaped() {
        // djf.12 F1: an unclosed fence, an HTML comment and a setext
        // underline each used to restyle or swallow what came after them.
        let config = Config::fake_app(isolated_options("block_starting_message_lines_are_escaped"));
        let exporter = Timeline::new(&config).unwrap();

        for (text, expected_body) in [
            (
                "here is the start of some code:\n```\nlet x = 1;",
                "here is the start of some code:\n\\```\nlet x = 1;",
            ),
            ("<!-- note", "\\<!-- note"),
            ("Results\n===\ndone", "Results\n\\===\ndone"),
        ] {
            let mut message = Config::fake_message_with_text(text);
            message.date = SAMPLE_DATE;
            message.is_from_me = true;
            assert_eq!(
                exporter.format_timeline_message(&message).unwrap(),
                format!("**5:29:42 PM — Me**\n\n{expected_body}\n\n"),
                "text {text:?}"
            );
        }
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

    // MARK: Replies (djf.12 F2)
    /// A parent message from me at 5:29:42 PM PT on Tuesday, May 17, 2022,
    /// with one reply.
    fn parent_message(rowid: i32, guid: &str) -> Message {
        let mut parent = Config::fake_message_with_text("parent");
        parent.rowid = rowid;
        parent.guid = guid.to_string();
        parent.date = SAMPLE_DATE;
        parent.is_from_me = true;
        parent.num_replies = 1;
        parent
    }

    #[test]
    fn reply_sent_on_another_day_shows_its_full_date_when_nested() {
        let config = seeded_config("reply_sent_on_another_day_shows_its_full_date_when_nested");
        let exporter = Timeline::new(&config).unwrap();
        let parent = parent_message(NEXT_DAY_PARENT_ROW, NEXT_DAY_PARENT_GUID);

        assert_eq!(
            exporter.format_timeline_message(&parent).unwrap(),
            "**5:29:42 PM — Me**\n\nparent\n\n> **Wed, May 18, 2022 5:29:42 PM — Me**\n>\n> reply sent the next day\n>\n\n"
        );
    }

    #[test]
    fn reply_sent_the_same_day_shows_only_its_time_when_nested() {
        let config = seeded_config("reply_sent_the_same_day_shows_only_its_time_when_nested");
        let exporter = Timeline::new(&config).unwrap();
        let parent = parent_message(PARENT_ROW, PARENT_GUID);

        let actual = exporter.format_timeline_message(&parent).unwrap();
        assert!(
            actual.starts_with("**5:29:42 PM — Me**\n\nparent\n\n> **5:29:42 PM — Me**\n>\n"),
            "got:\n{actual}"
        );
        assert!(!actual.contains("reply to"), "got:\n{actual}");
    }

    #[test]
    fn reply_under_its_own_day_names_when_its_parent_was_sent() {
        let config = seeded_config("reply_under_its_own_day_names_when_its_parent_was_sent");
        let exporter = Timeline::new(&config).unwrap();
        let reply = Message::from_guid(
            NEXT_DAY_REPLY_GUID,
            config.data_source.db(),
            &config.data_source.capabilities,
        )
        .unwrap();

        assert_eq!(
            exporter.format_timeline_message(&reply).unwrap(),
            "**5:29:42 PM — Me**\n\n*(reply to a message from Tue, May 17, 2022 5:29:42 PM)*\n\nreply sent the next day\n\n"
        );
    }

    #[test]
    fn reply_whose_parent_is_missing_says_an_earlier_message() {
        let config = seeded_config("reply_whose_parent_is_missing_says_an_earlier_message");
        let exporter = Timeline::new(&config).unwrap();
        let mut reply = Config::fake_message_with_text("orphaned reply");
        reply.date = SAMPLE_DATE;
        reply.is_from_me = true;
        reply.thread_originator_guid = Some("timeline-test-no-such-parent".to_string());

        assert_eq!(
            exporter.format_timeline_message(&reply).unwrap(),
            "**5:29:42 PM — Me**\n\n*(reply to an earlier message)*\n\norphaned reply\n\n"
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
