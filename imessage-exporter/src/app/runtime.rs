/*!
 The main app runtime.
*/

use std::{
    cmp::min,
    collections::{BTreeSet, HashMap, HashSet},
    fs::create_dir_all,
    path::{Path, PathBuf},
};

use fdlimit::raise_fd_limit;
use fs2::available_space;
use imessage_database::{
    tables::{
        attachment::Attachment,
        chat::Chat,
        chat_handle::ChatToHandle,
        handle::Handle,
        messages::Message,
        table::{ATTACHMENTS_DIR, Cacheable, ME, ORPHANED, UNKNOWN, get_db_size},
    },
    util::{
        dates::{format as format_date, get_local_time, get_offset, readable_diff},
        size::format_file_size,
    },
};

use crate::{
    Exporter, HTML, TXT, Timeline,
    app::{
        compatibility::attachment_manager::AttachmentManagerMode, contacts::Name,
        data_source::DataSource, error::RuntimeError, export_type::ExportType,
        options::{OPTION_CONVERSATION_WITH, Options},
        sanitizers::sanitize_filename,
    },
    exporters::exporter::ATTACHMENT_NO_FILENAME,
};

// Maximum length for filenames
const MAX_LENGTH: usize = 235;

// MARK: Config
/// Stores the application state and handles application lifecycle
pub struct Config {
    /// Map of iMessage database chatroom ID to chatroom information
    pub chatrooms: HashMap<i32, Chat>,
    /// Map of iMessage database chatroom ID to an internal unique chatroom ID
    pub real_chatrooms: HashMap<i32, i32>,
    /// Map of iMessage database chatroom ID to chatroom participant iMessage database handle IDs
    pub chatroom_participants: HashMap<i32, BTreeSet<i32>>,
    /// Map of deduplicated internal participant ID to contact info
    pub participants: HashMap<i32, Name>,
    /// Map of iMessage database handle ID to an internal unique participant ID, used to generate `participants`
    pub real_participants: HashMap<i32, i32>,
    /// Messages that are tapbacks (reactions) to other messages
    pub tapbacks: HashMap<String, HashMap<usize, Vec<Message>>>,
    /// Translated message GUIDs
    pub translated_messages: HashSet<String>,
    /// App configuration options
    pub options: Options,
    /// Global date offset used by the iMessage database:
    pub offset: i64,
    /// Data source for the application
    pub data_source: DataSource,
}

impl Config {
    /// Get the chatroom and its deduplicated ID for a message, if available
    pub fn conversation(&self, message: &Message) -> Option<(&Chat, &i32)> {
        match message.chat_id.or(message.deleted_from) {
            Some(chat_id) => {
                if let Some(chatroom) = self.chatrooms.get(&chat_id) {
                    self.real_chatrooms.get(&chat_id).map(|id| (chatroom, id))
                } else {
                    eprintln!("Chat ID {chat_id} does not exist in chat table!");
                    None
                }
            }
            // No chat_id provided
            None => None,
        }
    }

    /// Get the attachment path for the current session
    pub fn attachment_path(&self) -> PathBuf {
        let mut path = self.options.export_path.clone();
        path.push(ATTACHMENTS_DIR);
        path
    }

    /// Get the attachment path for a specific chat ID
    pub fn conversation_attachment_path(&self, chat_id: Option<i32>) -> String {
        if let Some(chat_id) = chat_id
            && let Some(real_id) = self.real_chatrooms.get(&chat_id)
        {
            return real_id.to_string();
        }
        String::from(ORPHANED)
    }

    /// Generate a file path for an attachment
    ///
    /// If the attachment was copied, use that path
    /// if not, default to the filename
    pub fn message_attachment_path(&self, attachment: &Attachment) -> String {
        // Build a relative filepath from the fully qualified one on the `Attachment`
        match &attachment.copied_path {
            Some(path) => {
                if let Ok(relative_path) = path.strip_prefix(&self.options.export_path) {
                    return relative_path.display().to_string();
                }
                path.display().to_string()
            }
            None => attachment
                .resolved_attachment_path(
                    &self.options.platform,
                    &self.options.db_path,
                    self.options.attachment_root.as_deref(),
                )
                .unwrap_or_else(|| {
                    attachment
                        .filename()
                        .unwrap_or(ATTACHMENT_NO_FILENAME)
                        .to_string()
                }),
        }
    }

    /// Get a relative path for the provided file.
    pub fn relative_path(&self, path: &Path) -> String {
        if let Ok(relative_path) = path.strip_prefix(&self.options.export_path) {
            return relative_path.display().to_string();
        }
        path.display().to_string()
    }

    // MARK: Filenames
    /// Get a filename for a chat, possibly using cached data.
    ///
    /// If the chat has an assigned name, use that, truncating if necessary.
    ///
    /// If it does not, first try and make a flat list of its members. Failing that, use the unique `chat_identifier` field.
    pub fn filename(&self, chatroom: &Chat) -> String {
        // Calculate effective max length accounting for export path
        let export_path_len = self.options.export_path.as_os_str().len();
        let max_len = MAX_LENGTH.saturating_sub(export_path_len + 1);

        let mut filename = match &chatroom.display_name() {
            // If there is a display name, use that
            Some(name) => {
                let truncated_len = name.floor_char_boundary(min(max_len, name.len()));
                format!(
                    "{} - {}",
                    &name[..truncated_len],
                    // Get the deduplicated chat ID to ensure the filename is unique, even if the group name is not
                    self.real_chatrooms
                        .get(&chatroom.rowid)
                        .unwrap_or(&chatroom.rowid)
                )
            }
            // Fallback if there is no name set
            None => {
                if let Some(participants) = self.chatroom_participants.get(&chatroom.rowid) {
                    self.filename_from_participants(participants)
                } else {
                    eprintln!(
                        "Found error: message chat ID {} has no members!",
                        chatroom.rowid
                    );
                    chatroom.chat_identifier.clone()
                }
            }
        };

        // Add the extension to the filename
        if let Some(export_type) = &self.options.export_type {
            filename.push_str(export_type.extension());
        }

        sanitize_filename(&filename)
    }

    /// Generate a filename from a set of participants, truncating if the name is too long
    ///
    /// - All names:
    ///   - Contact 1, Contact 2
    /// - Truncated Names
    ///   - Contact 1, Contact 2, ... Contact 13 and 4 others
    fn filename_from_participants(&self, participants: &BTreeSet<i32>) -> String {
        // Calculate effective max length accounting for export path
        let export_path_len = self.options.export_path.as_os_str().len();
        let max_len = MAX_LENGTH.saturating_sub(export_path_len + 1);

        let mut added = 0;
        let mut out_s = String::with_capacity(max_len);
        for participant_id in participants {
            let participant_details = match self.resolve_participant(*participant_id) {
                Some(name) => name.details.as_str(),
                None => UNKNOWN,
            };

            if participant_details.len() + out_s.len() < max_len {
                if !out_s.is_empty() {
                    out_s.push_str(", ");
                }
                out_s.push_str(participant_details);
                added += 1;
            } else {
                let extra = format!(", and {} others", participants.len() - added);
                let space_remaining = extra.len() + out_s.len();
                if space_remaining >= max_len {
                    let start = out_s.floor_char_boundary(max_len.saturating_sub(extra.len()));
                    out_s.replace_range(start.., &extra);
                } else if out_s.is_empty() {
                    let truncated_len = participant_details
                        .floor_char_boundary(min(max_len, participant_details.len()));
                    out_s.push_str(&participant_details[..truncated_len]);
                } else {
                    out_s.push_str(&extra);
                }
                break;
            }
        }
        out_s
    }

    // MARK: Init
    /// Create a new instance of the application
    ///
    pub fn new(options: Options) -> Result<Config, RuntimeError> {
        let data_source = DataSource::from(&options)?;

        eprintln!("Building cache...");
        eprintln!("  [1/5] Caching chats...");
        let chatrooms = Chat::cache(data_source.db())?;

        eprintln!("  [2/5] Caching chatrooms...");
        let chatroom_participants = ChatToHandle::cache(data_source.db())?;
        let chat_handle_lookup = ChatToHandle::get_chat_lookup_map(data_source.db())?;
        let real_chatrooms = ChatToHandle::dedupe(&chatroom_participants, &chat_handle_lookup)?;

        eprintln!("  [3/5] Caching participants...");
        let participants = Handle::cache(data_source.db())?;
        let real_participants = Handle::dedupe(&participants);
        let participants_map = data_source
            .contacts_index
            .build_participants_map(&participants, &real_participants);

        eprintln!("  [4/5] Caching tapbacks...");
        let tapbacks = Message::cache(data_source.db())?;

        eprintln!("  [5/5] Caching translations...");
        // Translations are not available in older database versions, so we default to an empty set
        let translated_messages = Message::cache_translations(data_source.db()).unwrap_or_default();
        eprintln!("Cache built!");

        Ok(Config {
            chatrooms,
            real_chatrooms,
            chatroom_participants,
            real_participants,
            participants: participants_map,
            tapbacks,
            translated_messages,
            options,
            offset: get_offset(),
            data_source,
        })
    }

    // MARK: Filters
    /// Convert participant filter strings into chat IDs and apply them to the
    /// query context.
    ///
    /// Two filter modes are supported and unioned:
    ///
    /// 1. `--conversation-filter` / `-t`: loose-OR. A chat is selected if it
    ///    contains at least one participant matching any of the comma-separated
    ///    tokens.
    /// 2. `--conversation-with` / `-w`: exact match. Each repeated value lists
    ///    the comma-separated participants of one specific conversation; a chat
    ///    is selected when its deduplicated non-self participant set equals the
    ///    set the value resolves to. Multiple `-w` values are unioned.
    pub(crate) fn resolve_filtered_handles(&mut self) {
        let has_filter = self.options.conversation_filter.is_some();
        let has_with = !self.options.conversation_with.is_empty();
        if !has_filter && !has_with {
            return;
        }

        let mut included_chatrooms: BTreeSet<i32> = BTreeSet::new();
        let mut included_handles: BTreeSet<i32> = BTreeSet::new();

        // Loose-OR filter (`-t`)
        if let Some(conversation_filter) = &self.options.conversation_filter {
            let parsed_handle_filter = conversation_filter.split(',').collect::<Vec<&str>>();

            self.participants.iter().for_each(|(_, handle_name)| {
                for included_name in &parsed_handle_filter {
                    if handle_name.contains(included_name) {
                        included_handles.extend(&handle_name.handle_ids);
                    }
                }
            });

            self.chatroom_participants
                .iter()
                .for_each(|(chat_id, participants)| {
                    if !participants.is_disjoint(&included_handles) {
                        included_chatrooms.insert(*chat_id);
                    }
                });
        }

        // Exact-match filter (`-w`)
        if has_with {
            let exact_chats = self.resolve_exact_match_chats(&mut included_handles);
            included_chatrooms.extend(exact_chats);
        }

        self.options
            .query_context
            .set_selected_handle_ids(included_handles);

        self.options
            .query_context
            .set_selected_chat_ids(included_chatrooms);

        self.log_filtered_handles_and_chats();
    }

    /// Resolve every `--conversation-with` group to the chat IDs whose
    /// deduplicated non-self participant set matches the group exactly.
    ///
    /// As a side effect, every raw handle id of every resolved participant is
    /// added to `included_handles` so diagnostic logging in
    /// [`Config::log_filtered_handles_and_chats`] reflects the full filter.
    fn resolve_exact_match_chats(&self, included_handles: &mut BTreeSet<i32>) -> BTreeSet<i32> {
        let mut included: BTreeSet<i32> = BTreeSet::new();

        for group_str in &self.options.conversation_with {
            let tokens: Vec<&str> = group_str
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if tokens.is_empty() {
                continue;
            }

            // Resolve each token to the set of deduped participant ids whose
            // Name contains the token. Track raw handle ids so we can also
            // surface them through `included_handles` for diagnostic logging.
            let mut resolved_deduped: BTreeSet<i32> = BTreeSet::new();
            let mut all_raw_handles: Vec<i32> = Vec::new();
            let mut any_token_unresolved = false;

            for token in &tokens {
                let mut token_matches: Vec<(i32, &Name)> = Vec::new();
                for (deduped_id, name) in &self.participants {
                    if name.contains(token) {
                        token_matches.push((*deduped_id, name));
                    }
                }
                if token_matches.is_empty() {
                    eprintln!(
                        "Warning: --{OPTION_CONVERSATION_WITH} token `{token}` did not match any participant",
                    );
                    any_token_unresolved = true;
                    continue;
                }
                if token_matches.len() > 1 {
                    eprintln!(
                        "Warning: --{OPTION_CONVERSATION_WITH} token `{token}` matched {} participants; treating them as a combined participant set",
                        token_matches.len()
                    );
                }
                for (deduped_id, name) in token_matches {
                    resolved_deduped.insert(deduped_id);
                    all_raw_handles.extend(&name.handle_ids);
                }
            }

            // If any token failed to resolve, the user's intent for this
            // group is ambiguous — skip the whole group rather than match a
            // smaller set than they asked for.
            if any_token_unresolved || resolved_deduped.is_empty() {
                continue;
            }

            for (chat_id, raw_participants) in &self.chatroom_participants {
                let chat_deduped: BTreeSet<i32> = raw_participants
                    .iter()
                    // Exclude handle 0 ("Me") since `-w` describes the
                    // *other* participants of the conversation.
                    .filter(|h| **h != 0)
                    .filter_map(|h| self.real_participants.get(h).copied())
                    .collect();

                if chat_deduped == resolved_deduped {
                    included.insert(*chat_id);
                    included_handles.extend(&all_raw_handles);
                }
            }
        }
        included
    }

    /// If we set some filtered chatrooms, emit how many will be included in the export
    fn log_filtered_handles_and_chats(&self) {
        if let (Some(selected_handle_ids), Some(selected_chat_ids)) = (
            &self.options.query_context.selected_handle_ids,
            &self.options.query_context.selected_chat_ids,
        ) {
            let unique_handle_ids: HashSet<Option<&i32>> = selected_handle_ids
                .iter()
                .map(|handle_id| self.real_participants.get(handle_id))
                .collect();

            let mut unique_chat_ids: HashSet<String> = HashSet::new();
            for selected_chat_id in selected_chat_ids {
                if let Some(participants) = self.chatroom_participants.get(selected_chat_id) {
                    unique_chat_ids.insert(self.filename_from_participants(participants));
                }
            }

            eprintln!(
                "Filtering for {} handle{} across {} chatrooms...",
                unique_handle_ids.len(),
                if unique_handle_ids.len() == 1 {
                    ""
                } else {
                    "s"
                },
                unique_chat_ids.len()
            );
        }
    }

    /// Ensure there is available disk space for the requested export
    fn ensure_free_space(&self) -> Result<(), RuntimeError> {
        // Export size is usually about 6% the size of the db;
        // we divide by 10 to over-estimate about 10% of the total size
        // for some safe headroom
        let total_db_size = get_db_size(Path::new(
            self.data_source
                .db()
                .path()
                .ok_or(RuntimeError::FileNameError)?,
        ))?;
        let mut estimated_export_size = total_db_size / 10;

        let free_space_at_location = available_space(&self.options.export_path)?;

        // Validate that there is enough disk space free to write the export
        if let AttachmentManagerMode::Disabled = self.options.attachment_manager.mode {
            if estimated_export_size >= free_space_at_location {
                return Err(RuntimeError::NotEnoughAvailableSpace(
                    estimated_export_size,
                    free_space_at_location,
                ));
            }
        } else {
            let total_attachment_size = Attachment::get_total_attachment_bytes(
                self.data_source.db(),
                &self.options.query_context,
            )?;
            estimated_export_size += total_attachment_size;
            if estimated_export_size >= free_space_at_location {
                return Err(RuntimeError::NotEnoughAvailableSpace(
                    estimated_export_size,
                    free_space_at_location,
                ));
            }
        }

        println!(
            "Estimated export size: {}",
            format_file_size(estimated_export_size)
        );

        Ok(())
    }

    // MARK: Diagnostic
    /// Handles diagnostic tests for database
    fn run_diagnostic(&self) -> Result<(), RuntimeError> {
        println!("\niMessage Database Diagnostics\n");

        // Handle diagnostics
        let handle_diag = Handle::run_diagnostic(self.data_source.db())?;
        println!("Handle diagnostic data:");
        println!("    Total handles: {}", handle_diag.total_handles);
        if handle_diag.handles_with_multiple_ids > 0 {
            println!(
                "    Handles with more than one ID: {}",
                handle_diag.handles_with_multiple_ids
            );
        }
        if handle_diag.total_duplicated > 0 {
            println!(
                "    Total duplicated handles: {}",
                handle_diag.total_duplicated
            );
        }

        // Message diagnostics
        let message_diag = Message::run_diagnostic(self.data_source.db())?;
        println!("Message diagnostic data:");
        println!("    Total messages: {}", message_diag.total_messages);
        if message_diag.messages_without_chat > 0 {
            println!(
                "    Messages not associated with a chat: {}",
                message_diag.messages_without_chat
            );
        }
        if message_diag.messages_in_multiple_chats > 0 {
            println!(
                "    Messages belonging to more than one chat: {}",
                message_diag.messages_in_multiple_chats
            );
        }
        if message_diag.recoverable_messages > 0 {
            println!(
                "    Recoverable deleted messages: {}",
                message_diag.recoverable_messages
            );
        }
        if let (Some(first), Some(last)) = (
            message_diag.first_message_date,
            message_diag.last_message_date,
        ) && let (Ok(first_date), Ok(last_date)) = (
            get_local_time(first, self.offset),
            get_local_time(last, self.offset),
        ) {
            println!(
                "    Date range: {} to {}\n                {}",
                format_date(&first_date),
                format_date(&last_date),
                readable_diff(&first_date, &last_date).unwrap_or_else(|| "N/A".to_string()),
            );
        }

        // Attachment diagnostics
        let attach_diag = Attachment::run_diagnostic(
            self.data_source.db(),
            &self.options.db_path,
            &self.options.platform,
            self.options.attachment_root.as_deref(),
        )?;
        if attach_diag.total_attachments > 0 {
            println!("Attachment diagnostic data:");
            println!("    Total attachments: {}", attach_diag.total_attachments);
            println!(
                "        Data referenced in table: {}",
                format_file_size(attach_diag.total_bytes_referenced)
            );
            println!(
                "        Data present on disk: {}",
                format_file_size(attach_diag.total_bytes_on_disk)
            );
            if attach_diag.missing_files > 0 {
                println!(
                    "    Missing files: {} ({:.0}%)",
                    attach_diag.missing_files,
                    attach_diag.missing_percent().unwrap_or(0.0)
                );
                println!("        No path provided: {}", attach_diag.no_path_provided);
                println!("        No file located: {}", attach_diag.no_file_located());
            }
        }

        // Chat/thread diagnostics
        let chat_diag = ChatToHandle::run_diagnostic(self.data_source.db())?;
        println!("Thread diagnostic data:");
        println!("    Total chats: {}", chat_diag.total_chats);
        if chat_diag.total_duplicated > 0 {
            println!("    Total duplicated chats: {}", chat_diag.total_duplicated);
        }
        if chat_diag.chats_with_no_handles > 0 {
            println!(
                "    Chats with no handles: {}",
                chat_diag.chats_with_no_handles
            );
        }

        // Global Diagnostics
        println!("Global diagnostic data:");

        let total_db_size = get_db_size(Path::new(
            self.data_source
                .db()
                .path()
                .ok_or(RuntimeError::FileNameError)?,
        ))?;
        println!(
            "    Total database size: {}",
            format_file_size(total_db_size)
        );

        // For each handle in the participants list, count how many have matches in the contacts index
        let total_resolved =
            self.participants.iter().fold(
                0,
                |acc, (_, name)| {
                    if name.full.is_empty() { acc } else { acc + 1 }
                },
            );

        println!(
            "    Handles with resolved names: {}/{} ({}%)",
            total_resolved,
            self.participants.len(),
            (total_resolved as f32 / self.participants.len() as f32 * 100.0).round()
        );

        println!("\nEnvironment Diagnostics\n");
        self.options.attachment_manager.diagnostic();

        Ok(())
    }

    // MARK: Startup
    /// Start the app given the provided set of options. This will either run
    /// diagnostic tests on the database or export data to the specified file type.
    pub fn start(&self) -> Result<(), RuntimeError> {
        if self.options.diagnostic {
            self.run_diagnostic()?;
        } else if let Some(export_type) = &self.options.export_type {
            // Ensure that if we want to filter on things, we have stuff to filter for
            if let Some(filters) = &self.options.conversation_filter
                && !self.options.query_context.has_filters()
            {
                return Err(RuntimeError::InvalidOptions(format!(
                    "Selected filter `{filters}` does not match any participants!"
                )));
            }

            // Ensure the path we want to export to exists
            create_dir_all(&self.options.export_path)?;

            // Ensure the path we want to copy attachments to exists, if requested
            if !matches!(
                self.options.attachment_manager.mode,
                AttachmentManagerMode::Disabled
            ) {
                create_dir_all(self.attachment_path())?;
            }

            // Ensure there is enough free disk space to write the export
            if !self.options.ignore_disk_space {
                self.ensure_free_space()?;
            }

            // Ensure we have enough file handles to export
            let _ = raise_fd_limit();

            // Create exporter, pass it data we care about, then kick it off
            match export_type {
                ExportType::Html => {
                    HTML::new(self)?.iter_messages()?;
                }
                ExportType::Txt => {
                    TXT::new(self)?.iter_messages()?;
                }
                ExportType::Timeline => {
                    Timeline::new(self)?.iter_messages()?;
                }
            }
        }
        println!("Done!");
        Ok(())
    }

    /// Determine who sent a message
    pub fn who<'a, 'b: 'a>(
        &'a self,
        handle_id: Option<i32>,
        is_from_me: bool,
        destination_caller_id: &'b Option<String>,
    ) -> &'a str {
        if is_from_me {
            if self.options.use_caller_id {
                return destination_caller_id.as_deref().unwrap_or(ME);
            }
            return self.options.custom_name.as_deref().unwrap_or(ME);
        } else if let Some(handle_id) = handle_id {
            return match self.resolve_participant(handle_id) {
                Some(contact) => contact.get_display_name(),
                None => UNKNOWN,
            };
        }
        UNKNOWN
    }

    /// Resolve a participant name from a handle ID
    fn resolve_participant(&self, handle_id: i32) -> Option<&Name> {
        if let Some(internal_id) = self.real_participants.get(&handle_id) {
            return self.participants.get(internal_id);
        }
        None
    }
}

// MARK: Test Config
#[cfg(test)]
impl Config {
    pub fn fake_app(options: Options) -> Config {
        let data_source = DataSource::from(&options).unwrap();

        Config {
            chatrooms: HashMap::new(),
            real_chatrooms: HashMap::new(),
            chatroom_participants: HashMap::new(),
            participants: HashMap::new(),
            real_participants: HashMap::new(),
            tapbacks: HashMap::new(),
            translated_messages: HashSet::new(),
            options,
            offset: get_offset(),
            data_source,
        }
    }

    pub fn fake_message() -> Message {
        Message {
            rowid: i32::default(),
            guid: String::default(),
            text: None,
            service: Some("iMessage".to_string()),
            handle_id: Some(i32::default()),
            destination_caller_id: None,
            subject: None,
            date: i64::default(),
            date_read: i64::default(),
            date_delivered: i64::default(),
            is_from_me: false,
            is_read: false,
            item_type: 0,
            other_handle: None,
            share_status: false,
            share_direction: None,
            group_title: None,
            group_action_type: 0,
            associated_message_guid: None,
            associated_message_type: Some(i32::default()),
            balloon_bundle_id: None,
            expressive_send_style_id: None,
            thread_originator_guid: None,
            thread_originator_part: None,
            date_edited: 0,
            associated_message_emoji: None,
            chat_id: None,
            num_attachments: 0,
            deleted_from: None,
            num_replies: 0,
            components: vec![],
            edited_parts: None,
        }
    }

    pub(crate) fn fake_attachment() -> Attachment {
        Attachment {
            rowid: 0,
            filename: Some("a/b/c/d.jpg".to_string()),
            uti: Some("public.png".to_string()),
            mime_type: Some("image/png".to_string()),
            transfer_name: Some("d.jpg".to_string()),
            total_bytes: 100,
            is_sticker: false,
            hide_attachment: 0,
            emoji_description: None,
            copied_path: None,
        }
    }
}

// MARK: Tests
#[cfg(test)]
mod filename_tests {
    use crate::{
        Config, Options,
        app::{contacts::Name, runtime::MAX_LENGTH},
    };

    use imessage_database::tables::chat::Chat;

    use std::collections::BTreeSet;

    pub fn fake_chat() -> Chat {
        Chat {
            rowid: 0,
            chat_identifier: "Default".to_string(),
            service_name: Some(String::new()),
            display_name: None,
        }
    }

    #[test]
    fn can_create() {
        let mut options = Options::fake_options(crate::app::export_type::ExportType::Html);
        // Disable the export
        options.export_type = None;
        let app = Config::fake_app(options);
        app.start().unwrap();
    }

    #[test]
    fn can_get_filename_good() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create participant data
        app.participants.insert(10, Name::fake_name("Person 10"));
        app.participants.insert(11, Name::fake_name("Person 11"));
        app.real_participants.insert(10, 10);
        app.real_participants.insert(11, 11);

        // Add participants
        let mut people = BTreeSet::new();
        people.insert(10);
        people.insert(11);

        // Get filename
        let filename = app.filename_from_participants(&people);
        assert_eq!(filename, "Person 10, Person 11".to_string());
        assert!(filename.len() <= MAX_LENGTH);
    }

    #[test]
    fn can_get_filename_long_multiple() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create participant data
        app.participants.insert(
            10,
            Name::fake_name("Person With An Extremely and Excessively Long Name 10"),
        );
        app.participants.insert(
            11,
            Name::fake_name("Person With An Extremely and Excessively Long Name 11"),
        );
        app.participants.insert(
            12,
            Name::fake_name("Person With An Extremely and Excessively Long Name 12"),
        );
        app.participants.insert(
            13,
            Name::fake_name("Person With An Extremely and Excessively Long Name 13"),
        );
        app.participants.insert(
            14,
            Name::fake_name("Person With An Extremely and Excessively Long Name 14"),
        );
        app.participants.insert(
            15,
            Name::fake_name("Person With An Extremely and Excessively Long Name 15"),
        );
        app.participants.insert(
            16,
            Name::fake_name("Person With An Extremely and Excessively Long Name 16"),
        );
        app.participants.insert(
            17,
            Name::fake_name("Person With An Extremely and Excessively Long Name 17"),
        );
        app.real_participants.insert(10, 10);
        app.real_participants.insert(11, 11);
        app.real_participants.insert(12, 12);
        app.real_participants.insert(13, 13);
        app.real_participants.insert(14, 14);
        app.real_participants.insert(15, 15);
        app.real_participants.insert(16, 16);
        app.real_participants.insert(17, 17);

        // Add participants
        let mut people = BTreeSet::new();
        people.insert(10);
        people.insert(11);
        people.insert(12);
        people.insert(13);
        people.insert(14);
        people.insert(15);
        people.insert(16);
        people.insert(17);

        // Get filename
        let filename = app.filename_from_participants(&people);
        assert_eq!(filename, "Person With An Extremely and Excessively Long Name 10, Person With An Extremely and Excessively Long Name 11, Person With An Extremely and Excessively Long Name 12, Person With An Extremely and Excessively Long Name , and 4 others".to_string());
        assert!(filename.len() <= MAX_LENGTH);
    }

    #[test]
    fn can_get_filename_single_long() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create participant data
        app.participants.insert(10, Name::fake_name("He slipped his key into the lock, and we all very quietly entered the cell. The sleeper half turned, and then settled down once more into a deep slumber. Holmes stooped to the water-jug, moistened his sponge, and then rubbed it twice vigorously across and down the prisoner's face."));
        app.real_participants.insert(10, 10);

        // Add 1 person
        let mut people = BTreeSet::new();
        people.insert(10);

        // Get filename
        let filename = app.filename_from_participants(&people);
        assert_eq!(filename, "He slipped his key into the lock, and we all very quietly entered the cell. The sleeper half turned, and then settled down once more into a deep slumber. Holmes stooped to the water-jug, moistened his sponge, and then rubbed it tw".to_string());
        assert!(filename.len() <= MAX_LENGTH);
    }

    #[test]
    fn can_get_filename_chat_display_name_long() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let app = Config::fake_app(options);

        // Create chat
        let mut chat = fake_chat();
        chat.display_name = Some("Life is infinitely stranger than anything which the mind of man could invent. We would not dare to conceive the things which are really mere commonplaces of existence. If we could fly out of that window hand in hand, hover over this great city, gently remove the roofs".to_string());

        // Get filename
        let filename = app.filename(&chat);
        assert_eq!(
            filename,
            "Life is infinitely stranger than anything which the mind of man could invent. We would not dare to conceive the things which are really mere commonplaces of existence. If we could fly out of that window hand in hand, hover over th - 0.html"
        );
    }

    #[test]
    fn can_get_filename_chat_display_name_normal() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let app = Config::fake_app(options);

        // Create chat
        let mut chat = fake_chat();
        chat.display_name = Some("Test Chat Name".to_string());

        // Get filename
        let filename = app.filename(&chat);
        assert_eq!(filename, "Test Chat Name - 0.html");
    }

    #[test]
    fn can_get_filename_chat_display_name_short() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let app = Config::fake_app(options);

        // Create chat
        let mut chat = fake_chat();
        chat.display_name = Some("🤠".to_string());

        // Get filename
        let filename = app.filename(&chat);
        assert_eq!(filename, "🤠 - 0.html");
    }

    #[test]
    fn can_get_filename_chat_participants() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create chat
        let chat = fake_chat();

        // Create participant data
        app.participants.insert(10, Name::fake_name("Person 10"));
        app.participants.insert(11, Name::fake_name("Person 11"));
        app.real_participants.insert(10, 10);
        app.real_participants.insert(11, 11);

        // Add participants
        let mut people = BTreeSet::new();
        people.insert(10);
        people.insert(11);
        app.chatroom_participants.insert(chat.rowid, people);

        // Get filename
        let filename = app.filename(&chat);
        assert_eq!(filename, "Person 10, Person 11.html");
    }

    #[test]
    fn can_get_filename_chat_no_participants() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let app = Config::fake_app(options);

        // Create chat
        let chat = fake_chat();

        // Get filename
        let filename = app.filename(&chat);
        assert_eq!(filename, "Default.html");
    }

    #[test]
    fn can_get_filename_chat_display_name_truncated_emoji() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let app = Config::fake_app(options);

        // Create a display name that is exactly at the boundary with a multi-byte emoji
        // Each 🤠 is 4 bytes. Fill enough to force truncation at an emoji boundary.
        let emoji_name: String = "🤠".repeat(60); // 240 bytes, exceeds MAX_LENGTH
        let mut chat = fake_chat();
        chat.display_name = Some(emoji_name);

        // Should not panic, and the result should be valid UTF-8
        let filename = app.filename(&chat);
        assert!(filename.len() <= MAX_LENGTH + 20); // suffix " - 0.html" adds some
        // Verify it's valid UTF-8 (would fail to compile/run if not)
        assert!(filename.ends_with(".html"));
    }

    #[test]
    fn can_get_filename_single_long_emoji() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create a participant with a name full of 4-byte emoji
        let emoji_name: String = "🌍".repeat(60); // 240 bytes
        app.participants.insert(10, Name::fake_name(&emoji_name));
        app.real_participants.insert(10, 10);

        let mut people = BTreeSet::new();
        people.insert(10);

        // Should not panic and should produce valid UTF-8
        let filename = app.filename_from_participants(&people);
        assert!(filename.len() <= MAX_LENGTH);
        // Verify the truncation happened on a char boundary
        for c in filename.chars() {
            assert!(c == '🌍');
        }
    }

    #[test]
    fn can_get_filename_multiple_long_emoji() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create participants with emoji names long enough to trigger the "and N others" truncation
        for i in 10..18 {
            let emoji_name: String = "🎵".repeat(30); // 120 bytes each
            app.participants.insert(i, Name::fake_name(&emoji_name));
            app.real_participants.insert(i, i);
        }

        let mut people = BTreeSet::new();
        for i in 10..18 {
            people.insert(i);
        }

        // Should not panic and should produce valid UTF-8 within the length limit
        let filename = app.filename_from_participants(&people);
        assert!(filename.len() <= MAX_LENGTH);
    }

    #[test]
    fn can_get_filename_cjk_truncation() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // CJK characters are 3 bytes each; test truncation mid-character
        let cjk_name: String = "你".repeat(80); // 240 bytes
        app.participants.insert(10, Name::fake_name(&cjk_name));
        app.real_participants.insert(10, 10);

        let mut people = BTreeSet::new();
        people.insert(10);

        let filename = app.filename_from_participants(&people);
        assert!(filename.len() <= MAX_LENGTH);
        // All characters should be valid
        for c in filename.chars() {
            assert!(c == '你');
        }
    }
}

#[cfg(test)]
mod who_tests {
    use crate::{
        Config, Options,
        app::{contacts::Name, runtime::filename_tests::fake_chat},
    };

    #[test]
    fn can_get_who_them() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create participant data
        app.participants.insert(10, Name::fake_name("Person 10"));
        app.real_participants.insert(10, 10);

        // Get participant name
        let who = app.who(Some(10), false, &None);
        assert_eq!(who, "Person 10".to_string());
    }

    #[test]
    fn can_get_who_them_missing() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let app = Config::fake_app(options);

        // Get participant name
        let who = app.who(Some(10), false, &None);
        assert_eq!(who, "Unknown".to_string());
    }

    #[test]
    fn can_get_who_me() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let app = Config::fake_app(options);

        // Get participant name
        let who = app.who(Some(0), true, &None);
        assert_eq!(who, "Me".to_string());
    }

    #[test]
    fn can_get_who_me_caller_id() {
        let mut options = Options::fake_options(crate::app::export_type::ExportType::Html);
        options.use_caller_id = true;
        let app = Config::fake_app(options);

        // Get participant name
        let caller_id = Some("test".to_string());
        let who = app.who(Some(0), true, &caller_id);
        assert_eq!(who, "test".to_string());
    }

    #[test]
    fn can_get_who_me_custom() {
        let mut options = Options::fake_options(crate::app::export_type::ExportType::Html);
        options.custom_name = Some("Name".to_string());
        let app = Config::fake_app(options);

        // Get participant name
        let who = app.who(Some(0), true, &None);
        assert_eq!(who, "Name".to_string());
    }

    #[test]
    fn can_get_who_none_me() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let app = Config::fake_app(options);

        // Get participant name
        let who = app.who(None, true, &None);
        assert_eq!(who, "Me".to_string());
    }

    #[test]
    fn can_get_who_me_none_caller_id() {
        let mut options = Options::fake_options(crate::app::export_type::ExportType::Html);
        options.use_caller_id = true;
        let app = Config::fake_app(options);

        // Get participant name
        let caller_id = Some("test".to_string());
        let who = app.who(None, true, &caller_id);
        assert_eq!(who, "test".to_string());
    }

    #[test]
    fn can_get_who_none_them() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let app = Config::fake_app(options);

        // Get participant name
        let who = app.who(None, false, &None);
        assert_eq!(who, "Unknown".to_string());
    }

    #[test]
    fn can_get_chat_valid() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create chat
        let chat = fake_chat();
        app.chatrooms.insert(chat.rowid, chat);
        app.real_chatrooms.insert(0, 0);

        // Create message
        let mut message = Config::fake_message();
        message.chat_id = Some(0);

        // Get filename
        let (_, id) = app.conversation(&message).unwrap();
        assert_eq!(id, &0);
    }

    #[test]
    fn can_get_chat_valid_deleted() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create chat
        let chat = fake_chat();
        app.chatrooms.insert(chat.rowid, chat);
        app.real_chatrooms.insert(0, 0);

        // Create message
        let mut message = Config::fake_message();
        message.chat_id = None;
        message.deleted_from = Some(0);

        // Get filename
        let (_, id) = app.conversation(&message).unwrap();
        assert_eq!(id, &0);
    }

    #[test]
    fn can_get_chat_invalid() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create chat
        let chat = fake_chat();
        app.chatrooms.insert(chat.rowid, chat);
        app.real_chatrooms.insert(0, 0);

        // Create message
        let mut message = Config::fake_message();
        message.chat_id = Some(1);

        // Get filename
        let room = app.conversation(&message);
        assert!(room.is_none());
    }

    #[test]
    fn can_get_chat_none() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create chat
        let chat = fake_chat();
        app.chatrooms.insert(chat.rowid, chat);
        app.real_chatrooms.insert(0, 0);

        // Create message
        let mut message = Config::fake_message();
        message.chat_id = None;
        message.deleted_from = None;

        // Get filename
        let room = app.conversation(&message);
        assert!(room.is_none());
    }
}

#[cfg(test)]
mod directory_tests {
    use crate::{Config, Options};
    use std::path::PathBuf;

    #[test]
    fn can_get_valid_attachment_sub_dir() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create chatroom ID
        app.real_chatrooms.insert(0, 0);

        // Get subdirectory
        let sub_dir = app.conversation_attachment_path(Some(0));
        assert_eq!(String::from("0"), sub_dir);
    }

    #[test]
    fn can_get_invalid_attachment_sub_dir() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create chatroom ID
        app.real_chatrooms.insert(0, 0);

        // Get subdirectory
        let sub_dir = app.conversation_attachment_path(Some(1));
        assert_eq!(String::from("orphaned"), sub_dir);
    }

    #[test]
    fn can_get_missing_attachment_sub_dir() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let mut app = Config::fake_app(options);

        // Create chatroom ID
        app.real_chatrooms.insert(0, 0);

        // Get subdirectory
        let sub_dir = app.conversation_attachment_path(None);
        assert_eq!(String::from("orphaned"), sub_dir);
    }

    #[test]
    fn can_get_path_not_copied() {
        let options = Options::fake_options(crate::app::export_type::ExportType::Html);
        let app = Config::fake_app(options);

        // Create attachment
        let attachment = Config::fake_attachment();

        let result = app.message_attachment_path(&attachment);
        let expected = String::from("a/b/c/d.jpg");
        assert_eq!(result, expected);
    }

    #[test]
    fn can_get_path_copied() {
        let mut options = Options::fake_options(crate::app::export_type::ExportType::Html);
        // Set an export path
        options.export_path = PathBuf::from("/Users/ReagentX/exports");

        let app = Config::fake_app(options);

        // Create attachment
        let mut attachment = Config::fake_attachment();
        let mut full_path = PathBuf::from("/Users/ReagentX/exports/attachments");
        full_path.push(attachment.filename().unwrap());
        attachment.copied_path = Some(full_path);

        let result = app.message_attachment_path(&attachment);
        let expected = String::from("attachments/d.jpg");
        assert_eq!(result, expected);
    }

    #[test]
    fn can_get_path_copied_bad() {
        let mut options = Options::fake_options(crate::app::export_type::ExportType::Html);
        // Set an export path
        options.export_path = PathBuf::from("/Users/ReagentX/exports");

        let app = Config::fake_app(options);

        // Create attachment
        let mut attachment = Config::fake_attachment();
        attachment.copied_path = Some(PathBuf::from(attachment.filename.as_ref().unwrap()));

        let result = app.message_attachment_path(&attachment);
        let expected = String::from("a/b/c/d.jpg");
        assert_eq!(result, expected);
    }
}

#[cfg(test)]
mod conversation_with_tests {
    use std::collections::BTreeSet;

    use crate::{
        Config, Options,
        app::{contacts::Name, export_type::ExportType},
    };

    /// Set up an app with three deduplicated participants and a fixed set of
    /// chatrooms used across the exact-match tests:
    ///
    /// - Chat 1: DM with Alice (handles {10})
    /// - Chat 2: Group with Alice + Bob (handles {10, 11})
    /// - Chat 3: DM with Bob (handles {11})
    /// - Chat 4: Group with Alice + Bob + Charlie (handles {10, 11, 12})
    ///
    /// Each participant's deduped id equals their raw handle id, which keeps
    /// the test setup simple. The dedup-specific test below sets up its own
    /// app where one person has two raw handles.
    fn fixture_with_three_people() -> Config {
        let options = Options::fake_options(ExportType::Html);
        let mut app = Config::fake_app(options);

        for (id, name) in [(10, "Alice"), (11, "Bob"), (12, "Charlie")] {
            app.participants.insert(id, Name::fake_name(name));
            app.real_participants.insert(id, id);
        }
        for (id, p) in app.participants.iter_mut() {
            p.handle_ids.insert(*id);
        }

        let mut chat_1 = BTreeSet::new();
        chat_1.insert(10);
        app.chatroom_participants.insert(1, chat_1);

        let mut chat_2 = BTreeSet::new();
        chat_2.insert(10);
        chat_2.insert(11);
        app.chatroom_participants.insert(2, chat_2);

        let mut chat_3 = BTreeSet::new();
        chat_3.insert(11);
        app.chatroom_participants.insert(3, chat_3);

        let mut chat_4 = BTreeSet::new();
        chat_4.insert(10);
        chat_4.insert(11);
        chat_4.insert(12);
        app.chatroom_participants.insert(4, chat_4);

        app
    }

    #[test]
    fn dm_exact_match_picks_only_the_dm() {
        let mut app = fixture_with_three_people();
        app.options.conversation_with = vec![String::from("Alice")];
        app.resolve_filtered_handles();

        // Only chat 1 (the Alice DM) should match — not the alice+bob group
        // and not the alice+bob+charlie group.
        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([1]))
        );
    }

    #[test]
    fn group_exact_match_requires_full_set_no_subset_or_superset() {
        let mut app = fixture_with_three_people();
        app.options.conversation_with = vec![String::from("Alice,Bob")];
        app.resolve_filtered_handles();

        // Only chat 2 matches: chat 1 is missing Bob, chat 4 has an extra
        // participant (Charlie), and chat 3 is missing Alice.
        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([2]))
        );
    }

    #[test]
    fn group_exact_match_three_people() {
        let mut app = fixture_with_three_people();
        app.options.conversation_with = vec![String::from("Alice,Bob,Charlie")];
        app.resolve_filtered_handles();

        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([4]))
        );
    }

    #[test]
    fn token_order_does_not_matter() {
        let mut app = fixture_with_three_people();
        app.options.conversation_with = vec![String::from("Bob,Alice")];
        app.resolve_filtered_handles();

        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([2]))
        );
    }

    #[test]
    fn whitespace_around_tokens_is_ignored() {
        let mut app = fixture_with_three_people();
        app.options.conversation_with = vec![String::from(" Alice ,  Bob ")];
        app.resolve_filtered_handles();

        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([2]))
        );
    }

    #[test]
    fn multiple_conversation_with_flags_union_results() {
        let mut app = fixture_with_three_people();
        app.options.conversation_with = vec![
            // Alice DM
            String::from("Alice"),
            // Alice + Bob group
            String::from("Alice,Bob"),
        ];
        app.resolve_filtered_handles();

        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([1, 2]))
        );
    }

    #[test]
    fn unknown_token_yields_empty_selection() {
        let mut app = fixture_with_three_people();
        app.options.conversation_with = vec![String::from("DoesNotExist")];
        app.resolve_filtered_handles();

        // No matches → we expect no chat ids selected at all (None when the
        // set is empty per QueryContext::set_selected_chat_ids semantics).
        assert!(
            app.options
                .query_context
                .selected_chat_ids
                .as_ref()
                .map_or(true, BTreeSet::is_empty)
        );
    }

    #[test]
    fn empty_group_string_is_ignored() {
        let mut app = fixture_with_three_people();
        // Empty string and all-whitespace string should both be silently
        // ignored rather than matching a degenerate chat.
        app.options.conversation_with =
            vec![String::new(), String::from("  ,  "), String::from("Alice")];
        app.resolve_filtered_handles();

        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([1]))
        );
    }

    #[test]
    fn combined_with_conversation_filter_unions_loose_or_with_exact_match() {
        let mut app = fixture_with_three_people();
        // Loose-OR filter: any chat with Charlie (i.e. just chat 4)
        app.options.conversation_filter = Some(String::from("Charlie"));
        // Exact match: alice DM (chat 1) and alice+bob group (chat 2)
        app.options.conversation_with =
            vec![String::from("Alice"), String::from("Alice,Bob")];

        app.resolve_filtered_handles();

        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([1, 2, 4]))
        );
    }

    #[test]
    fn deduplicated_handles_treated_as_one_person_in_exact_match() {
        // Alice has two raw handles (phone + email) that map to the same
        // deduplicated person id (10). One chat references only her email
        // handle, the other references only her phone. Both should match
        // `-w Alice`.
        let options = Options::fake_options(ExportType::Html);
        let mut app = Config::fake_app(options);

        let mut alice = Name::fake_name("Alice");
        alice.handle_ids.insert(10);
        alice.handle_ids.insert(100);
        app.participants.insert(10, alice);

        // Both raw handle IDs collapse to deduped id 10.
        app.real_participants.insert(10, 10);
        app.real_participants.insert(100, 10);

        // Chat 1: alice via phone handle (10) only - DM
        let mut chat_1 = BTreeSet::new();
        chat_1.insert(10);
        app.chatroom_participants.insert(1, chat_1);

        // Chat 2: alice via email handle (100) only - DM
        let mut chat_2 = BTreeSet::new();
        chat_2.insert(100);
        app.chatroom_participants.insert(2, chat_2);

        // Chat 3: alice via both raw handles (rare but representable) - DM
        let mut chat_3 = BTreeSet::new();
        chat_3.insert(10);
        chat_3.insert(100);
        app.chatroom_participants.insert(3, chat_3);

        app.options.conversation_with = vec![String::from("Alice")];
        app.resolve_filtered_handles();

        // All three chats are 1-on-1 DMs with Alice once handles are deduped.
        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([1, 2, 3]))
        );
    }

    #[test]
    fn exact_match_excludes_self_handle_zero() {
        // Some chats include handle id 0 (the user themselves). The exact
        // match should ignore handle 0 when comparing against the user's
        // requested participant list, since `-w` describes the *other*
        // participants of the conversation.
        let options = Options::fake_options(ExportType::Html);
        let mut app = Config::fake_app(options);

        app.participants.insert(10, Name::fake_name("Alice"));
        app.real_participants.insert(10, 10);
        for (id, p) in app.participants.iter_mut() {
            p.handle_ids.insert(*id);
        }
        // Handle 0 (Me) is not a user-visible participant in normal exports;
        // for this test we don't list it in `participants` but we DO put it
        // in the chat's handle set to mirror older databases that do.
        app.real_participants.insert(0, 0);

        let mut chat_1 = BTreeSet::new();
        chat_1.insert(0); // self
        chat_1.insert(10); // alice
        app.chatroom_participants.insert(1, chat_1);

        app.options.conversation_with = vec![String::from("Alice")];
        app.resolve_filtered_handles();

        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([1]))
        );
    }
}

#[cfg(test)]
mod chat_filter_tests {
    use std::collections::BTreeSet;

    use crate::{
        Config, Options,
        app::{contacts::Name, export_type::ExportType},
    };

    #[test]
    fn can_generate_filter_string_multiple() {
        let mut options = Options::fake_options(ExportType::Html);
        options.conversation_filter = Some(String::from("Person 10,Person 11,Person 12"));

        let mut app = Config::fake_app(options);

        // Add some test data
        app.participants.insert(10, Name::fake_name("Person 10")); // Included
        app.participants.insert(11, Name::fake_name("Person 11")); // Included
        app.participants.insert(12, Name::fake_name("Person 12")); // Included
        app.participants.insert(13, Name::fake_name("Person 13")); // Excluded

        // Set the chatroom participant IDs
        for (id, participant) in app.participants.iter_mut() {
            participant.handle_ids.insert(*id);
        }

        // Chatroom 1: Included
        let mut chatroom_1 = BTreeSet::new();
        chatroom_1.insert(10);
        app.chatroom_participants.insert(1, chatroom_1);

        // Chatroom 2: Included
        let mut chatroom_2 = BTreeSet::new();
        chatroom_2.insert(11);
        app.chatroom_participants.insert(2, chatroom_2);

        // Chatroom 3: Included
        let mut chatroom_3 = BTreeSet::new();
        chatroom_3.insert(12);
        app.chatroom_participants.insert(3, chatroom_3);

        // Chatroom 4: Excluded
        let mut chatroom_4 = BTreeSet::new();
        chatroom_4.insert(13);
        app.chatroom_participants.insert(4, chatroom_4);

        // Chatroom 5: Included
        let mut chatroom_5 = BTreeSet::new();
        chatroom_5.insert(10);
        chatroom_5.insert(11);
        app.chatroom_participants.insert(5, chatroom_5);

        // Chatroom 6: Included
        let mut chatroom_6 = BTreeSet::new();
        chatroom_6.insert(12);
        chatroom_6.insert(13); // Even though this person is excluded, the above person is
        app.chatroom_participants.insert(6, chatroom_6);

        app.resolve_filtered_handles();
        // For the test, sort the output so it is always the same

        assert_eq!(
            app.options.query_context.selected_handle_ids,
            Some(BTreeSet::from([10, 11, 12]))
        );
        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([1, 2, 3, 5, 6]))
        );
    }

    #[test]
    fn can_generate_filter_string_single() {
        let mut options = Options::fake_options(ExportType::Html);
        options.conversation_filter = Some(String::from("Person 13"));

        let mut app = Config::fake_app(options);

        // Add some test data
        app.participants.insert(10, Name::fake_name("Person 10")); // Excluded
        app.participants.insert(11, Name::fake_name("Person 11")); // Excluded
        app.participants.insert(12, Name::fake_name("Person 12")); // Excluded
        app.participants.insert(13, Name::fake_name("Person 13")); // Included

        // Set the chatroom participant IDs
        for (id, participant) in app.participants.iter_mut() {
            participant.handle_ids.insert(*id);
        }

        // Chatroom 1: Excluded
        let mut chatroom_1 = BTreeSet::new();
        chatroom_1.insert(10);
        app.chatroom_participants.insert(1, chatroom_1);

        // Chatroom 2: Excluded
        let mut chatroom_2 = BTreeSet::new();
        chatroom_2.insert(11);
        app.chatroom_participants.insert(2, chatroom_2);

        // Chatroom 3: Excluded
        let mut chatroom_3 = BTreeSet::new();
        chatroom_3.insert(12);
        app.chatroom_participants.insert(3, chatroom_3);

        // Chatroom 4: Included
        let mut chatroom_4 = BTreeSet::new();
        chatroom_4.insert(13);
        app.chatroom_participants.insert(4, chatroom_4);

        // Chatroom 5: Excluded
        let mut chatroom_5 = BTreeSet::new();
        chatroom_5.insert(10);
        chatroom_5.insert(11);
        app.chatroom_participants.insert(5, chatroom_5);

        // Chatroom 6: Included
        let mut chatroom_6 = BTreeSet::new();
        chatroom_6.insert(12);
        chatroom_6.insert(13); // Even though this person is excluded, the above person is
        app.chatroom_participants.insert(6, chatroom_6);

        app.resolve_filtered_handles();
        // For the test, sort the output so it is always the same

        assert_eq!(
            app.options.query_context.selected_handle_ids,
            Some(BTreeSet::from([13]))
        );
        assert_eq!(
            app.options.query_context.selected_chat_ids,
            Some(BTreeSet::from([4, 6]))
        );
    }
}
