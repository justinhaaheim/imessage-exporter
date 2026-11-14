/*!
 Markdown rendering of app balloons (link previews, Apple Music shares,
 polls, Apple Pay, and the other app message kinds) for the timeline export.

 This file is fork-owned. Upstream's [`dispatch_app_balloon`] decides which
 kind of balloon a message carries and parses its payload; this module only
 supplies the [`BalloonFormatter`] leaves. Each leaf shows the same fields,
 in the same order, as upstream's TXT template for that balloon
 (`txt/balloons.rs` and the templates in `txt/templates/balloons`).

 Markdown shape: one italic type label line (`*Link*`, `*Poll*`, or
 `*<app name>*` for other apps), then one line per present field. URLs are
 Markdown autolinks (`<https://…>`). The label replaces TXT's own naming
 words (`"<app> message:"`, `" transaction:"`, `"Photo album:"`), so those
 words are not repeated. Field text is written as parsed, the same way the
 timeline writes message text, with one exception: a line that Markdown
 would read as a heading, a code fence or an HTML block gets a backslash
 before its first marker character (djf.9, djf.12 F1; see
 [`escape_block_starts`](crate::exporters::timeline::escape_block_starts)).
*/

use std::borrow::Cow;

use imessage_database::{
    message_types::{
        app::AppMessage, app_store::AppStoreMessage, business_chat::BusinessMessage,
        collaboration::CollaborationMessage, digital_touch::DigitalTouchMessage,
        handwriting::HandwrittenMessage, music::MusicMessage, placemark::PlacemarkMessage,
        polls::Poll, url::URLMessage,
    },
    tables::{attachment::Attachment, messages::Message},
};

use crate::{
    app::{error::RuntimeError, runtime::Config},
    exporters::{
        formatter::BalloonFormatter,
        shared::{
            balloon::{dispatch_app_balloon, resolve_check_in_footer},
            text::OptionalText,
        },
        timeline::{escape_block_starts_after, is_blank_line},
    },
};

/// Width, in characters, of the ASCII rendering of a handwritten message.
/// Matches the TXT exporter's `render_ascii(40)`.
const HANDWRITING_ASCII_WIDTH: usize = 40;

/// Type label for a generic app balloon with neither an app name nor a bundle ID.
const GENERIC_APP_LABEL: &str = "App";

// MARK: Entry point
/// Render the app balloon of `message` as Markdown.
///
/// Returns upstream's error unchanged when the payload is missing or cannot
/// be parsed; the caller decides what to show instead.
pub(crate) fn render_app_balloon(
    config: &Config,
    message: &Message,
    attachments: &[Attachment],
) -> Result<String, RuntimeError> {
    dispatch_app_balloon(&TimelineBalloons { config }, message, attachments, config)
}

// MARK: Formatter
/// The timeline's [`BalloonFormatter`]. Holds only the runtime config, so
/// building one has no side effects (unlike constructing a TXT exporter).
struct TimelineBalloons<'a> {
    config: &'a Config,
}

impl BalloonFormatter for TimelineBalloons<'_> {
    fn format_url(&self, msg: &Message, balloon: &URLMessage) -> String {
        let mut out = Balloon::new("Link");
        out.url(balloon.get_url().or(msg.text.as_deref()))
            .field(balloon.title)
            .field(balloon.summary);
        out.finish()
    }

    fn format_music(&self, balloon: &MusicMessage) -> String {
        let mut out = Balloon::new("Music");
        if let Some(lyrics) = balloon.lyrics.as_deref() {
            out.line("Lyrics:");
            for line in lyrics {
                out.line(line);
            }
            // TXT separates the lyrics from the track fields with blank lines.
            out.line("");
        }
        out.field(balloon.track_name)
            .field(balloon.album)
            .field(balloon.artist)
            .url(balloon.url);
        out.finish()
    }

    fn format_collaboration(&self, balloon: &CollaborationMessage) -> String {
        let mut out = Balloon::new("Collaboration");
        out.field(balloon.app_name.or(balloon.bundle_id))
            .field(balloon.title)
            .url(balloon.get_url());
        out.finish()
    }

    fn format_app_store(&self, balloon: &AppStoreMessage) -> String {
        let mut out = Balloon::new("App Store");
        out.field(balloon.app_name)
            .field(balloon.description)
            .field(balloon.platform)
            .field(balloon.genre)
            .url(balloon.get_url());
        out.finish()
    }

    fn format_placemark(&self, balloon: &PlacemarkMessage) -> String {
        let place = &balloon.placemark;
        let mut out = Balloon::new("Location");
        out.field(balloon.place_name)
            .url(balloon.get_url())
            .field(place.name)
            .field(place.address)
            .field(place.state)
            .field(place.city)
            .field(place.iso_country_code)
            .field(place.postal_code)
            .field(place.country)
            .field(place.street)
            .field(place.sub_administrative_area)
            .field(place.sub_locality);
        out.finish()
    }

    fn format_handwriting(&self, _: &Message, balloon: &HandwrittenMessage) -> String {
        // The timeline never copies attachment files, so it always shows the
        // ASCII rendering, as TXT does with attachment copying disabled. A
        // fenced block keeps Markdown from reading the `*` strokes and the
        // leading spaces as emphasis, lists or indented code.
        let ascii = balloon.render_ascii(HANDWRITING_ASCII_WIDTH);
        let mut out = Balloon::new("Handwriting");
        out.raw_line("```")
            .raw_line(ascii.trim_end_matches('\n'))
            .raw_line("```");
        out.finish()
    }

    fn format_digital_touch(&self, _: &Message, balloon: &DigitalTouchMessage) -> String {
        let text = balloon.render_text();
        let mut out = Balloon::new("Digital Touch");
        out.field(Some(text.as_str()));
        out.finish()
    }

    fn format_apple_pay(&self, balloon: &AppMessage) -> String {
        let mut out = Balloon::new("Apple Pay");
        out.field(balloon.caption)
            .line(present(balloon.ldtext).unwrap_or("unknown amount"));
        out.finish()
    }

    fn format_fitness(&self, balloon: &AppMessage) -> String {
        let mut out = Balloon::new("Fitness");
        out.field(balloon.app_name)
            .line(present(balloon.ldtext).unwrap_or("unknown workout"));
        out.finish()
    }

    fn format_slideshow(&self, balloon: &AppMessage) -> String {
        let mut out = Balloon::new("Slideshow");
        out.field(balloon.ldtext).url(balloon.url);
        out.finish()
    }

    fn format_find_my(&self, balloon: &AppMessage) -> String {
        let mut out = Balloon::new("Find My");
        out.field(balloon.app_name).field(balloon.ldtext);
        out.finish()
    }

    fn format_check_in(&self, balloon: &AppMessage) -> String {
        // TXT falls back to the caption "Check In" only to name the balloon;
        // the type label already does that here, so a missing caption is
        // simply omitted.
        let footer = resolve_check_in_footer(balloon, &self.config.options.timezone);
        let mut out = Balloon::new("Check In");
        out.field(balloon.caption).field(footer.as_deref());
        out.finish()
    }

    fn format_poll(&self, poll: &Poll) -> String {
        let mut out = Balloon::new("Poll");
        for option in poll.order.iter().filter_map(|id| poll.options.get(id)) {
            out.line(&format!("- {} ({})", option.text, option.votes.len()));
            for vote in &option.votes {
                out.line(&format!("  - {}", vote.voter));
            }
        }
        out.finish()
    }

    fn format_business(&self, balloon: &BusinessMessage) -> String {
        let mut out = Balloon::new("Business");
        match balloon {
            BusinessMessage::QuickReply(quick_reply) => {
                out.field(quick_reply.summary.as_deref());
                for (index, option) in quick_reply.options.iter().enumerate() {
                    let selected = quick_reply.selected_index == Some(index);
                    out.line(&format!("- {}{}", option.title, check_mark(selected)));
                }
            }
            BusinessMessage::FormResponse(form) => {
                out.field(form.summary.as_deref());
                for answer in &form.answers {
                    out.line(&format!(
                        "- {} → {}",
                        answer.question,
                        answer.answers.join(", ")
                    ));
                }
            }
            BusinessMessage::FormRequest(form) => {
                out.field(form.title.as_deref())
                    .field(form.subtitle.as_deref());
            }
            BusinessMessage::ListPicker(list_picker) => {
                out.field(list_picker.summary.as_deref());
                for item in &list_picker.items {
                    let subtitle = present(item.subtitle.as_deref())
                        .map(|subtitle| format!(" ({subtitle})"))
                        .unwrap_or_default();
                    out.line(&format!(
                        "- {}{subtitle}{}",
                        item.title,
                        check_mark(item.selected)
                    ));
                }
            }
        }
        out.finish()
    }

    fn format_generic_app(
        &self,
        balloon: &AppMessage,
        bundle_id: &str,
        _: &[Attachment],
        _: &Message,
    ) -> String {
        let name = present(balloon.app_name)
            .or_else(|| present(Some(bundle_id)))
            .unwrap_or(GENERIC_APP_LABEL);
        let mut out = Balloon::new(name);
        out.field(balloon.title)
            .field(balloon.subtitle)
            .field(balloon.caption)
            .field(balloon.subcaption)
            .field(balloon.trailing_caption)
            .field(balloon.trailing_subcaption);
        out.finish()
    }
}

// MARK: Builder
/// Accumulates one balloon: the italic type label, then one line per field.
struct Balloon(String);

impl Balloon {
    fn new(label: &str) -> Self {
        Self(format!("*{label}*"))
    }

    /// Append `text` as its own line, escaping any line of it that would read
    /// as a Markdown heading or open a code fence that `text` does not close
    /// or an HTML block (djf.9, djf.12 F1, djf.16 R2). `text` gets no prefix
    /// or suffix, so a fence it closes stays closed. Each line sits directly
    /// under the one before it, so a
    /// first line of only `=` or `-` under a line that is not blank would
    /// make that line a heading, and is escaped too.
    fn line(&mut self, text: &str) -> &mut Self {
        let last_line = self.0.rsplit(['\n', '\r']).next().unwrap_or_default();
        let escaped = escape_block_starts_after(text, !is_blank_line(last_line));
        self.raw_line(&escaped)
    }

    /// Append `text` as its own line, unchanged. For lines inside a fenced
    /// code block, where a backslash would show literally.
    fn raw_line(&mut self, text: &str) -> &mut Self {
        self.0.push('\n');
        self.0.push_str(text);
        self
    }

    /// Append a field when it is present and not empty, as TXT's templates do.
    fn field(&mut self, value: Option<&str>) -> &mut Self {
        match present(value) {
            Some(text) => self.line(text),
            None => self,
        }
    }

    /// Append a URL field as a Markdown autolink when it is present.
    fn url(&mut self, value: Option<&str>) -> &mut Self {
        match present(value) {
            Some(url) => self.line(&autolink(url)),
            None => self,
        }
    }

    /// The balloon without a trailing newline, the shape TXT's balloons have.
    fn finish(mut self) -> String {
        while self.0.ends_with('\n') {
            self.0.pop();
        }
        self.0
    }
}

// MARK: Helpers
/// `value` without empty strings, via upstream's [`OptionalText`].
fn present(value: Option<&str>) -> Option<&str> {
    OptionalText::from(value).get()
}

/// The selection suffix TXT's business templates print.
fn check_mark(selected: bool) -> &'static str {
    if selected { " ✓" } else { "" }
}

/// Wrap `url` in `<…>` when it is a CommonMark autolink: a scheme of 2 to 32
/// characters (a letter, then letters, digits, `+`, `.` or `-`), a colon, and
/// no spaces, `<`, `>` or control characters. Anything else, such as a bare
/// `example.com` from the message text, is returned unchanged, because
/// Markdown would show `<example.com>` literally or read it as an HTML tag.
fn autolink(url: &str) -> Cow<'_, str> {
    let Some((scheme, rest)) = url.split_once(':') else {
        return Cow::Borrowed(url);
    };
    let mut scheme_chars = scheme.chars();
    let valid_scheme = (2..=32).contains(&scheme.len())
        && scheme_chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && scheme_chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    let valid_rest = !rest
        .chars()
        .any(|c| c.is_ascii_control() || matches!(c, ' ' | '<' | '>'));
    if valid_scheme && valid_rest {
        Cow::Owned(format!("<{url}>"))
    } else {
        Cow::Borrowed(url)
    }
}

// MARK: Tests
#[cfg(test)]
mod tests {
    use std::{collections::HashMap, env::current_dir, fs};

    use super::{TimelineBalloons, autolink};
    use crate::{
        Config, Options, app::export_type::ExportType, exporters::formatter::BalloonFormatter,
    };
    use imessage_database::message_types::{
        app::AppMessage,
        app_store::AppStoreMessage,
        business_chat::{
            BusinessMessage, FormAnswer, FormRequest, FormResponse, ListPicker, ListPickerItem,
            QuickReply, QuickReplyOption,
        },
        collaboration::CollaborationMessage,
        digital_touch::DigitalTouchMessage,
        handwriting::HandwrittenMessage,
        music::MusicMessage,
        placemark::{Placemark, PlacemarkMessage},
        polls::{Poll, PollOption, PollOptionID, PollVote},
        url::URLMessage,
    };

    fn config() -> Config {
        Config::fake_app(Options::fake_options(ExportType::Timeline))
    }

    fn fixture(relative: &str) -> Vec<u8> {
        let path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data")
            .join(relative);
        fs::read(path).unwrap()
    }

    fn app_message() -> AppMessage<'static> {
        AppMessage {
            image: Some("image"),
            url: Some("https://example.com/app"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        }
    }

    fn empty_app_message() -> AppMessage<'static> {
        AppMessage {
            image: None,
            url: None,
            title: None,
            subtitle: None,
            caption: None,
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: None,
            ldtext: None,
        }
    }

    #[test]
    fn url_shows_link_title_and_summary() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let balloon = URLMessage {
            title: Some("title"),
            summary: Some("summary"),
            url: Some("https://example.com/page"),
            original_url: Some("https://example.com/original"),
            item_type: None,
            images: vec![],
            icons: vec![],
            site_name: None,
            placeholder: false,
        };

        let actual = formatter.format_url(&Config::fake_message(), &balloon);

        assert_eq!(actual, "*Link*\n<https://example.com/page>\ntitle\nsummary");
    }

    #[test]
    fn url_without_metadata_shows_the_message_text() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let mut msg = Config::fake_message();
        msg.text = Some("https://example.com/from-text".to_string());

        let actual = formatter.format_url(&msg, &URLMessage::default());

        assert_eq!(actual, "*Link*\n<https://example.com/from-text>");
    }

    #[test]
    fn music_shows_track_album_artist_and_link() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let balloon = MusicMessage {
            url: Some("https://music.example.com/track"),
            preview: Some("https://music.example.com/preview.m4a"),
            artist: Some("artist"),
            album: Some(""),
            track_name: Some("track_name"),
            lyrics: None,
        };

        let actual = formatter.format_music(&balloon);

        assert_eq!(
            actual,
            "*Music*\ntrack_name\nartist\n<https://music.example.com/track>"
        );
    }

    #[test]
    fn music_lyrics_come_before_the_track_fields() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let balloon = MusicMessage {
            url: None,
            preview: None,
            artist: Some("artist"),
            album: None,
            track_name: Some("track_name"),
            lyrics: Some(vec!["a", "b"]),
        };

        let actual = formatter.format_music(&balloon);

        assert_eq!(actual, "*Music*\nLyrics:\na\nb\n\ntrack_name\nartist");
    }

    #[test]
    fn collaboration_shows_app_title_and_link() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let balloon = CollaborationMessage {
            original_url: Some("https://example.com/original"),
            url: None,
            title: Some("Board"),
            creation_date: None,
            bundle_id: Some("com.example.board"),
            app_name: None,
        };

        let actual = formatter.format_collaboration(&balloon);

        assert_eq!(
            actual,
            "*Collaboration*\ncom.example.board\nBoard\n<https://example.com/original>"
        );
    }

    #[test]
    fn app_store_shows_fields_in_txt_order() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let balloon = AppStoreMessage {
            url: Some("https://apps.example.com/app"),
            app_name: Some("app_name"),
            original_url: None,
            description: Some("description"),
            platform: Some("platform"),
            genre: Some("genre"),
        };

        let actual = formatter.format_app_store(&balloon);

        assert_eq!(
            actual,
            "*App Store*\napp_name\ndescription\nplatform\ngenre\n<https://apps.example.com/app>"
        );
    }

    #[test]
    fn placemark_shows_fields_in_txt_order() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let balloon = PlacemarkMessage {
            url: Some("https://maps.example.com/?q=1"),
            original_url: None,
            place_name: Some("Place"),
            placemark: Placemark {
                name: Some("name"),
                address: Some("address"),
                state: Some("state"),
                city: Some("city"),
                iso_country_code: Some("iso_country_code"),
                postal_code: Some("postal_code"),
                country: Some("country"),
                street: Some("street"),
                sub_administrative_area: Some("sub_administrative_area"),
                sub_locality: Some("sub_locality"),
            },
        };

        let actual = formatter.format_placemark(&balloon);

        assert_eq!(
            actual,
            "*Location*\nPlace\n<https://maps.example.com/?q=1>\nname\naddress\nstate\ncity\niso_country_code\npostal_code\ncountry\nstreet\nsub_administrative_area\nsub_locality"
        );
    }

    #[test]
    fn handwriting_shows_ascii_art_in_a_fenced_block() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let balloon =
            HandwrittenMessage::from_payload(&fixture("handwritten_message/handwriting.bin"))
                .unwrap();
        let ascii = String::from_utf8(fixture("handwritten_message/handwriting.ascii")).unwrap();

        let actual = formatter.format_handwriting(&Config::fake_message(), &balloon);

        let expected = format!("*Handwriting*\n```\n{}\n```", ascii.trim_end_matches('\n'));
        assert_eq!(actual, expected);
    }

    #[test]
    fn digital_touch_shows_its_description() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let balloon =
            DigitalTouchMessage::from_payload(&fixture("digital_touch_message/kiss.bin")).unwrap();

        let actual = formatter.format_digital_touch(&Config::fake_message(), &balloon);

        assert_eq!(actual, "*Digital Touch*\nDigital Touch Kiss (1 kiss)");
    }

    #[test]
    fn apple_pay_shows_caption_and_amount() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };

        assert_eq!(
            formatter.format_apple_pay(&app_message()),
            "*Apple Pay*\ncaption\nldtext"
        );
        assert_eq!(
            formatter.format_apple_pay(&empty_app_message()),
            "*Apple Pay*\nunknown amount"
        );
    }

    #[test]
    fn fitness_shows_app_name_and_workout() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };

        assert_eq!(
            formatter.format_fitness(&app_message()),
            "*Fitness*\napp_name\nldtext"
        );
        assert_eq!(
            formatter.format_fitness(&empty_app_message()),
            "*Fitness*\nunknown workout"
        );
    }

    #[test]
    fn slideshow_shows_album_and_link() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };

        assert_eq!(
            formatter.format_slideshow(&app_message()),
            "*Slideshow*\nldtext\n<https://example.com/app>"
        );
        assert_eq!(
            formatter.format_slideshow(&empty_app_message()),
            "*Slideshow*"
        );
    }

    #[test]
    fn find_my_shows_app_name_and_status() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let mut balloon = empty_app_message();
        balloon.app_name = Some("Find My");
        balloon.ldtext = Some("Started Sharing Location");

        assert_eq!(
            formatter.format_find_my(&balloon),
            "*Find My*\nFind My\nStarted Sharing Location"
        );
    }

    #[test]
    fn check_in_shows_caption_and_footer() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let mut balloon = empty_app_message();
        balloon.url = Some("?messageType=1&interfaceVersion=1&sendDate=1697316869.688709");
        balloon.caption = Some("Check In: Timer Started");

        assert_eq!(
            formatter.format_check_in(&balloon),
            "*Check In*\nCheck In: Timer Started\nChecked in at Oct 14, 2023  1:54:29 PM"
        );
    }

    #[test]
    fn check_in_without_caption_or_metadata_is_just_the_label() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };

        assert_eq!(
            formatter.format_check_in(&empty_app_message()),
            "*Check In*"
        );
    }

    #[test]
    fn poll_lists_options_with_vote_counts_and_voters() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let id1: PollOptionID = "1".to_string();
        let id2: PollOptionID = "2".to_string();
        let mut options: HashMap<PollOptionID, PollOption> = HashMap::new();
        options.insert(
            id1.clone(),
            PollOption {
                text: "Rust".to_string(),
                creator: "alice".to_string(),
                votes: vec![],
            },
        );
        options.insert(
            id2.clone(),
            PollOption {
                text: "Go".to_string(),
                creator: "bob".to_string(),
                votes: vec![
                    PollVote {
                        voter: "alice".to_string(),
                        option_id: id2.clone(),
                    },
                    PollVote {
                        voter: "bob".to_string(),
                        option_id: id2.clone(),
                    },
                ],
            },
        );
        let poll = Poll {
            options,
            order: vec![id1, id2],
        };

        assert_eq!(
            formatter.format_poll(&poll),
            "*Poll*\n- Rust (0)\n- Go (2)\n  - alice\n  - bob"
        );
    }

    #[test]
    fn business_quick_reply_marks_the_selected_option() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let balloon = BusinessMessage::QuickReply(QuickReply {
            summary: Some("Replied to a question".to_string()),
            options: vec![
                QuickReplyOption {
                    title: "Yes".to_string(),
                },
                QuickReplyOption {
                    title: "No".to_string(),
                },
            ],
            selected_index: Some(0),
        });

        assert_eq!(
            formatter.format_business(&balloon),
            "*Business*\nReplied to a question\n- Yes ✓\n- No"
        );
    }

    #[test]
    fn business_forms_show_their_fields() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let request = BusinessMessage::FormRequest(FormRequest {
            title: Some("Report an Issue".to_string()),
            subtitle: Some("Tap to get started".to_string()),
        });
        let response = BusinessMessage::FormResponse(FormResponse {
            summary: Some("Here's my completed form".to_string()),
            answers: vec![FormAnswer {
                question: "Which days?".to_string(),
                answers: vec!["Monday".to_string(), "Friday".to_string()],
            }],
        });

        assert_eq!(
            formatter.format_business(&request),
            "*Business*\nReport an Issue\nTap to get started"
        );
        assert_eq!(
            formatter.format_business(&response),
            "*Business*\nHere's my completed form\n- Which days? → Monday, Friday"
        );
    }

    #[test]
    fn business_list_picker_shows_subtitles_and_selection() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let balloon = BusinessMessage::ListPicker(ListPicker {
            summary: Some("Select a Product".to_string()),
            items: vec![
                ListPickerItem {
                    title: "iPhone".to_string(),
                    subtitle: None,
                    selected: true,
                },
                ListPickerItem {
                    title: "AirPods".to_string(),
                    subtitle: Some("Wireless".to_string()),
                    selected: false,
                },
            ],
        });

        assert_eq!(
            formatter.format_business(&balloon),
            "*Business*\nSelect a Product\n- iPhone ✓\n- AirPods (Wireless)"
        );
    }

    #[test]
    fn generic_app_is_labelled_with_its_name() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };

        let actual =
            formatter.format_generic_app(&app_message(), "bundle_id", &[], &Config::fake_message());

        assert_eq!(
            actual,
            "*app_name*\ntitle\nsubtitle\ncaption\nsubcaption\ntrailing_caption\ntrailing_subcaption"
        );
    }

    #[test]
    fn generic_app_label_falls_back_to_bundle_id_then_app() {
        let config = config();
        let formatter = TimelineBalloons { config: &config };
        let mut balloon = empty_app_message();
        balloon.app_name = Some("");
        balloon.caption = Some("Your move.");

        assert_eq!(
            formatter.format_generic_app(
                &balloon,
                "com.example.game",
                &[],
                &Config::fake_message()
            ),
            "*com.example.game*\nYour move."
        );
        assert_eq!(
            formatter.format_generic_app(&balloon, "", &[], &Config::fake_message()),
            "*App*\nYour move."
        );
    }

    #[test]
    fn heading_shaped_field_lines_are_escaped() {
        // djf.9: a field line that reads as a heading gets a backslash.
        let config = config();
        let formatter = TimelineBalloons { config: &config };

        let mut balloon = empty_app_message();
        balloon.app_name = Some("Game");
        balloon.title = Some("# Title");
        balloon.caption = Some("Caption\n   ## second line");
        balloon.subcaption = Some("#hashtag");
        assert_eq!(
            formatter.format_generic_app(&balloon, "", &[], &Config::fake_message()),
            "*Game*\n\\# Title\nCaption\n   \\## second line\n#hashtag"
        );

        let music = MusicMessage {
            url: None,
            preview: None,
            artist: None,
            album: None,
            track_name: Some("## track"),
            lyrics: Some(vec!["# chorus", "verse"]),
        };
        assert_eq!(
            formatter.format_music(&music),
            "*Music*\nLyrics:\n\\# chorus\nverse\n\n\\## track"
        );

        // A link whose text is not a URL is written as is, then escaped.
        let url = URLMessage {
            title: None,
            summary: None,
            url: None,
            original_url: None,
            item_type: None,
            images: vec![],
            icons: vec![],
            site_name: None,
            placeholder: false,
        };
        let message = Config::fake_message_with_text("# not a link");
        assert_eq!(
            formatter.format_url(&message, &url),
            "*Link*\n\\# not a link"
        );
    }

    #[test]
    fn block_starting_field_lines_are_escaped() {
        // djf.12 F1: a field that opens a fence or an HTML block, or that
        // underlines the line above it, is escaped too.
        let config = config();
        let formatter = TimelineBalloons { config: &config };

        let mut balloon = empty_app_message();
        balloon.app_name = Some("Game");
        // Directly under the label line, so it would make the label a heading.
        balloon.title = Some("===");
        balloon.subtitle = Some("```");
        balloon.caption = Some("<!-- note");
        balloon.subcaption = Some("Total\n---");
        balloon.trailing_caption = Some("~~~ x");
        assert_eq!(
            formatter.format_generic_app(&balloon, "", &[], &Config::fake_message()),
            "*Game*\n\\===\n\\```\n\\<!-- note\nTotal\n\\---\n\\~~~ x"
        );

        // After the blank line that ends the lyrics, a line of dashes is a
        // rule, not an underline, so it is left alone.
        let music = MusicMessage {
            url: None,
            preview: None,
            artist: None,
            album: None,
            track_name: Some("---"),
            lyrics: Some(vec!["verse", "==="]),
        };
        assert_eq!(
            formatter.format_music(&music),
            "*Music*\nLyrics:\nverse\n\\===\n\n---"
        );
    }

    #[test]
    fn autolink_wraps_only_absolute_uris() {
        assert_eq!(
            autolink("https://example.com/a?b=c"),
            "<https://example.com/a?b=c>"
        );
        assert_eq!(
            autolink("mailto:someone@example.com"),
            "<mailto:someone@example.com>"
        );
        assert_eq!(autolink("example.com"), "example.com");
        assert_eq!(
            autolink("see https://example.com"),
            "see https://example.com"
        );
        assert_eq!(autolink("x:y"), "x:y");
        assert_eq!(
            autolink("https://example.com/<b>"),
            "https://example.com/<b>"
        );
    }
}
