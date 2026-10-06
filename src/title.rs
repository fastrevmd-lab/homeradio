/// Parse title metadata from various stream formats
/// Returns (artist, title) as Options
pub fn parse_title(raw: &str) -> (Option<String>, Option<String>) {
    if raw.trim().is_empty() {
        return (None, None);
    }

    // Try iHeart format: title="...",artist="..."
    if let Some((artist, title)) = parse_iheart_title_artist(raw) {
        return (Some(artist), Some(title));
    }

    // Try iHeart text format: [Artist - ]text="..." song_spot=...
    if let Some((artist, title)) = parse_iheart_text(raw) {
        return (artist, Some(title));
    }

    // Try plain "Artist - Title" format
    if let Some((artist, title)) = parse_plain_dash(raw) {
        return (Some(artist), Some(title));
    }

    // Unparsed key="value" metadata blob: never surface it to the UI
    if looks_like_metadata_blob(raw) {
        return (None, None);
    }

    // Nothing matched, return the raw string as title
    (None, Some(raw.to_string()))
}

/// True when `raw` is only a placeholder for the stream itself: the station URL
/// or its bare path tail (e.g. `zc2505`), which cliamp uses when a stream has no
/// metadata.
pub fn is_placeholder_title(raw: &str, url: Option<&str>) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return true;
    }
    let Some(url) = url else {
        return false;
    };
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let tail = without_query.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    trimmed == url || trimmed == without_query || (!tail.is_empty() && trimmed == tail)
}

fn looks_like_metadata_blob(s: &str) -> bool {
    s.contains("song_spot=") || s.contains("url=\"") || s.contains("MediaBaseId=")
}

fn parse_iheart_title_artist(s: &str) -> Option<(String, String)> {
    // Everything from the `url="..."` value onward holds nested quotes and
    // unrelated keys; only look at the part before it.
    let head = match s.find(",url=\"") {
        Some(index) => &s[..index],
        None => s,
    };
    let title = extract_quoted_value(head, "title=")?;
    let artist = extract_quoted_value(head, "artist=")?;
    Some((titlecase_if_all_caps(&artist), title))
}

/// `text="Title" song_spot="M" ...`, optionally preceded by `Artist - `.
fn parse_iheart_text(s: &str) -> Option<(Option<String>, String)> {
    if !s.contains("song_spot=") {
        return None;
    }
    let title = extract_quoted_value(s, "text=")?;
    let artist = s
        .find(" - text=\"")
        .map(|index| s[..index].trim())
        .filter(|artist| !artist.is_empty() && !artist.contains("=\""))
        .map(titlecase_if_all_caps);
    Some((artist, title))
}

fn parse_plain_dash(s: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = s.splitn(2, " - ").collect();
    if parts.len() == 2 {
        let artist = parts[0].trim();
        let title = parts[1].trim();
        if !artist.is_empty() && !title.is_empty() {
            return Some((titlecase_if_all_caps(artist), title.to_string()));
        }
    }
    None
}

/// Extract `key="value"` where `key` sits at the start of the string or after a
/// comma/space. The value ends at the first `"` that is followed by `,`, a space
/// or the end of the string, so stray inner quotes do not truncate it.
fn extract_quoted_value(s: &str, prefix: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(relative) = s[search_from..].find(prefix) {
        let start = search_from + relative;
        search_from = start + prefix.len();

        let anchored = start == 0 || matches!(s.as_bytes()[start - 1], b',' | b' ');
        if !anchored {
            continue;
        }
        let rest = &s[start + prefix.len()..];
        let Some(value_and_after) = rest.strip_prefix('"') else {
            continue;
        };
        for (index, character) in value_and_after.char_indices() {
            if character != '"' {
                continue;
            }
            let after = &value_and_after[index + 1..];
            if after.is_empty() || after.starts_with(',') || after.starts_with(' ') {
                return Some(value_and_after[..index].to_string());
            }
        }
        return None;
    }
    None
}

fn titlecase_if_all_caps(s: &str) -> String {
    if s.chars().filter(|c| c.is_alphabetic()).all(|c| c.is_uppercase()) {
        titlecase(s)
    } else {
        s.to_string()
    }
}

fn titlecase(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let mut result = first.to_uppercase().collect::<String>();
                    result.push_str(&chars.as_str().to_lowercase());
                    result
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iheart_title_artist() {
        let raw = r#"title="Rebel Yell",artist="BILLY IDOL",url="song_spot=...""#;
        let (artist, title) = parse_title(raw);
        assert_eq!(artist, Some("Billy Idol".to_string()));
        assert_eq!(title, Some("Rebel Yell".to_string()));
    }

    #[test]
    fn test_iheart_text_format() {
        let raw = r#"text="Spiderwebs" song_spot="M" ..."#;
        let (artist, title) = parse_title(raw);
        assert_eq!(artist, None);
        assert_eq!(title, Some("Spiderwebs".to_string()));
    }

    #[test]
    fn test_real_cliamp_artist_dash_text_format() {
        let raw = r#"Journey - text="Don't Stop Believin'" song_spot="M" MediaBaseId="1088710" itunesTrackId="0" amgTrackId="-1" amgArtistId="0" TAID="0" TPID="0" cartcutId="0" length="00:04:10" unsID="-1""#;
        let (artist, title) = parse_title(raw);
        assert_eq!(artist, Some("Journey".to_string()));
        assert_eq!(title, Some("Don't Stop Believin'".to_string()));

        let (artist, title) = parse_title(r#"JOURNEY - text="Faithfully" song_spot="M" MediaBaseId="1""#);
        assert_eq!(artist, Some("Journey".to_string()));
        assert_eq!(title, Some("Faithfully".to_string()));
    }

    #[test]
    fn test_plain_dash_format() {
        let raw = "The Killers - Mr. Brightside";
        let (artist, title) = parse_title(raw);
        assert_eq!(artist, Some("The Killers".to_string()));
        assert_eq!(title, Some("Mr. Brightside".to_string()));
    }

    #[test]
    fn test_plain_dash_all_caps() {
        let raw = "NIRVANA - Smells Like Teen Spirit";
        let (artist, title) = parse_title(raw);
        assert_eq!(artist, Some("Nirvana".to_string()));
        assert_eq!(title, Some("Smells Like Teen Spirit".to_string()));
    }

    #[test]
    fn test_real_cliamp_stevie_nicks_blob() {
        let raw = r#"title="Edge Of Seventeen {Just Like The White Winged Dove}",artist="STEVIE NICKS",url="song_spot="F" MediaBaseId="0" itunesTrackId="0" amgTrackId="-1" amgArtistId="0" TAID="0" TPID="43131402" cartcutId="0" amgArtworkURL="https://i.iheart.com/v3/catalog/track/43131402?ops=fit(200,200),format(%22jpeg%22)" length="00:04:29" unsID="-1" spotInstanceId="a015c878-ad55-48af-9e83-be370e5898b7"""#;
        let (artist, title) = parse_title(raw);
        assert_eq!(artist, Some("Stevie Nicks".to_string()));
        assert_eq!(
            title,
            Some("Edge Of Seventeen {Just Like The White Winged Dove}".to_string())
        );
    }

    #[test]
    fn test_unparseable_blob_is_not_surfaced() {
        let (artist, title) = parse_title(r#"url="song_spot="F" MediaBaseId="0""#);
        assert_eq!((artist, title), (None, None));
    }

    #[test]
    fn test_placeholder_title() {
        let url = Some("https://stream.revma.ihrhls.com/zc2505");
        assert!(is_placeholder_title("zc2505", url));
        assert!(is_placeholder_title("https://stream.revma.ihrhls.com/zc2505", url));
        assert!(!is_placeholder_title("Rebel Yell", url));
        assert!(!is_placeholder_title("zc2505", None));
    }

    #[test]
    fn test_unknown_format_passthrough() {
        let raw = "Some random text";
        let (artist, title) = parse_title(raw);
        assert_eq!(artist, None);
        assert_eq!(title, Some("Some random text".to_string()));
    }
}
