use super::{DEFAULT_MATCH_CONTEXT, DEFAULT_MATCH_LIMIT, MAX_MATCH_CONTEXT, MAX_MATCH_LIMIT};

#[derive(Debug)]
pub(crate) enum PagerCommand {
    Next { count: u64 },
    Skip { count: u64 },
    All,
    Range { start: usize, end: usize },
    Tail { spec: TailSpec },
    Search { pattern: SearchPattern },
    Where { pattern: SearchPattern },
    Matches { spec: MatchSpec },
    Hits { spec: HitSpec },
    SearchNext { count: u64 },
    SearchPrev { count: u64 },
    Goto { index: usize },
    Seek { spec: SeekSpec },
    Help,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TailSpec {
    Default,
    Bytes(u64),
    Lines(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SeekSpec {
    Offset(u64),
    Percent(u64),
    Line(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchPattern {
    pub(crate) pattern: String,
    pub(crate) case_insensitive_ascii: bool,
}

#[derive(Debug, Default, Clone)]
struct SearchFlags {
    case_insensitive_ascii: bool,
    context: Option<usize>,
    n_value: Option<String>,
}

#[derive(Debug, Clone)]
struct SearchArgs {
    pattern: String,
    case_insensitive_ascii: bool,
    context: usize,
    n_value: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum SearchCountFlag {
    Limit,
    Count,
}

impl SearchCountFlag {
    fn long_flag(self) -> &'static str {
        match self {
            SearchCountFlag::Limit => "--limit",
            SearchCountFlag::Count => "--count",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MatchSpec {
    pub(crate) pattern: Option<SearchPattern>,
    pub(crate) limit: usize,
    pub(crate) context: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HitSpec {
    pub(crate) pattern: SearchPattern,
    pub(crate) context: usize,
    pub(crate) count: u64,
}

fn strip_trailing_newlines(raw: &str) -> &str {
    let raw = raw.strip_suffix('\n').unwrap_or(raw);
    raw.strip_suffix('\r').unwrap_or(raw)
}

fn consume_one_whitespace(raw: &str) -> Option<&str> {
    let mut chars = raw.chars();
    let first = chars.next()?;
    first.is_whitespace().then_some(chars.as_str())
}

fn strip_word_prefix<'a>(raw: &'a str, word: &str) -> Option<&'a str> {
    let rest = raw.strip_prefix(word)?;
    if rest.is_empty()
        || rest
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_whitespace())
    {
        Some(rest)
    } else {
        None
    }
}

fn split_first_token(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_whitespace() {
            end = idx;
            break;
        }
    }
    let token = &trimmed[..end];
    let rest = &trimmed[end..];
    Some((token, rest))
}

fn parse_number_token(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

fn parse_limit_token(raw: &str) -> Option<usize> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lowercase = trimmed.to_ascii_lowercase();
    if lowercase == "all" || lowercase == "max" {
        return Some(MAX_MATCH_LIMIT);
    }
    let parsed = trimmed.parse::<u64>().ok()?;
    if parsed == 0 {
        return Some(MAX_MATCH_LIMIT);
    }
    Some(parsed as usize)
}

fn parse_search_flags(raw: &str, n_flag: SearchCountFlag) -> Option<(SearchFlags, &str)> {
    let mut rest = raw.trim_start();
    let mut flags = SearchFlags::default();

    loop {
        let (token, next) = split_first_token(rest)?;
        if token == "--" {
            rest = next;
            break;
        }
        if !token.starts_with('-') {
            rest = rest.trim_start();
            break;
        }

        match token {
            "-i" | "--ignore-case" => {
                flags.case_insensitive_ascii = true;
                rest = next;
            }
            "-n" => {
                let (value, tail) = split_first_token(next)?;
                flags.n_value = Some(value.to_string());
                rest = tail;
            }
            "-C" | "--context" => {
                let (value, tail) = split_first_token(next)?;
                let parsed = parse_number_token(value)?;
                flags.context = Some(parsed as usize);
                rest = tail;
            }
            _ if token == n_flag.long_flag() => {
                let (value, tail) = split_first_token(next)?;
                flags.n_value = Some(value.to_string());
                rest = tail;
            }
            _ => {
                if let Some(value) = token.strip_prefix("-n") {
                    flags.n_value = Some(value.to_string());
                    rest = next;
                } else {
                    let value = token.strip_prefix("-C")?;
                    let parsed = parse_number_token(value)?;
                    flags.context = Some(parsed as usize);
                    rest = next;
                }
            }
        }
    }

    Some((flags, rest))
}

fn parse_search_args(raw: &str, n_flag: SearchCountFlag) -> Option<SearchArgs> {
    let (flags, rest) = parse_search_flags(raw, n_flag)?;
    let pattern = rest.trim_start();
    if pattern.is_empty() {
        return None;
    }
    let context = flags
        .context
        .unwrap_or(DEFAULT_MATCH_CONTEXT)
        .min(MAX_MATCH_CONTEXT);
    Some(SearchArgs {
        pattern: pattern.to_string(),
        case_insensitive_ascii: flags.case_insensitive_ascii,
        context,
        n_value: flags.n_value,
    })
}

fn parse_match_spec(raw: &str) -> Option<MatchSpec> {
    let raw = raw.trim_start();
    if raw.is_empty() {
        return Some(MatchSpec {
            pattern: None,
            limit: DEFAULT_MATCH_LIMIT,
            context: DEFAULT_MATCH_CONTEXT,
        });
    }
    let (flags, rest) = parse_search_flags(raw, SearchCountFlag::Limit)?;
    let context = flags
        .context
        .unwrap_or(DEFAULT_MATCH_CONTEXT)
        .min(MAX_MATCH_CONTEXT);
    let limit = match &flags.n_value {
        Some(value) => parse_limit_token(value)?,
        None => DEFAULT_MATCH_LIMIT,
    };

    Some(MatchSpec {
        pattern: if rest.trim().is_empty() {
            None
        } else {
            Some(SearchPattern {
                pattern: rest.trim_start().to_string(),
                case_insensitive_ascii: flags.case_insensitive_ascii,
            })
        },
        limit: limit.clamp(1, MAX_MATCH_LIMIT),
        context,
    })
}

fn parse_hit_spec(raw: &str) -> Option<HitSpec> {
    let args = parse_search_args(raw, SearchCountFlag::Count)?;
    let count = match &args.n_value {
        Some(value) => parse_number_token(value)?.max(1),
        None => 1,
    };

    Some(HitSpec {
        pattern: SearchPattern {
            pattern: args.pattern,
            case_insensitive_ascii: args.case_insensitive_ascii,
        },
        context: args.context,
        count: count.clamp(1, MAX_MATCH_LIMIT as u64),
    })
}

impl PagerCommand {
    pub(crate) fn parse(input: &str) -> Option<Self> {
        let line = strip_trailing_newlines(input);
        if line.trim().is_empty() {
            return Some(Self::Next { count: 1 });
        }
        let trimmed = line.trim();

        // Pager commands are explicit to avoid collisions with backend code while pager is active.
        // Non-empty inputs must be prefixed with `:` to be interpreted as pager commands.
        let trimmed = {
            let rest = trimmed.strip_prefix(':')?;
            let rest = rest.trim_start();
            if rest.is_empty() {
                return None;
            }
            rest
        };

        if trimmed == "help" {
            return Some(Self::Help);
        }

        let leading = trimmed;
        if let Some(pattern) = leading.strip_prefix('/') {
            if pattern.is_empty() {
                return None;
            }

            let (case_insensitive_ascii, pattern) =
                if let Some(rest) = strip_word_prefix(pattern, "i") {
                    (true, consume_one_whitespace(rest)?)
                } else if let Some(rest) = strip_word_prefix(pattern, "I") {
                    (true, consume_one_whitespace(rest)?)
                } else {
                    (false, pattern)
                };

            if pattern.is_empty() {
                return None;
            }

            return Some(Self::Search {
                pattern: SearchPattern {
                    pattern: pattern.to_string(),
                    case_insensitive_ascii,
                },
            });
        }

        let mut parts = leading.split_whitespace();
        let head_raw = parts.next().unwrap_or_default();
        match head_raw {
            "next" => {
                let count = parts
                    .next()
                    .and_then(|part| part.parse::<u64>().ok())
                    .unwrap_or(1);
                Some(Self::Next {
                    count: count.max(1),
                })
            }
            "skip" => {
                let count = parts
                    .next()
                    .and_then(|part| part.parse::<u64>().ok())
                    .unwrap_or(1);
                Some(Self::Skip {
                    count: count.max(1),
                })
            }
            "where" => {
                let rest = leading.get(head_raw.len()..)?;
                let rest = consume_one_whitespace(rest)?;
                let (case_insensitive_ascii, rest) =
                    if let Some(after) = strip_word_prefix(rest, "-i") {
                        (true, consume_one_whitespace(after)?)
                    } else if let Some(after) = strip_word_prefix(rest, "--ignore-case") {
                        (true, consume_one_whitespace(after)?)
                    } else {
                        (false, rest)
                    };

                if rest.is_empty() {
                    return None;
                }
                Some(Self::Where {
                    pattern: SearchPattern {
                        pattern: rest.to_string(),
                        case_insensitive_ascii,
                    },
                })
            }
            "matches" => {
                let rest = leading.get(head_raw.len()..)?;
                parse_match_spec(rest).map(|spec| Self::Matches { spec })
            }
            "hits" => {
                let rest = leading.get(head_raw.len()..)?;
                parse_hit_spec(rest).map(|spec| Self::Hits { spec })
            }
            "a" => Some(Self::All),
            "range" => {
                let rest = leading.get(head_raw.len()..)?;
                parse_range_spec(rest).map(|(start, end)| Self::Range { start, end })
            }
            "tail" => {
                let arg = parts.next();
                Some(Self::Tail {
                    spec: arg.and_then(parse_tail_spec).unwrap_or(TailSpec::Default),
                })
            }
            "seek" => {
                let arg = parts.collect::<Vec<_>>().join(" ");
                parse_seek_spec(&arg).map(|spec| Self::Seek { spec })
            }
            "goto" => {
                let index = parts.next()?.parse::<usize>().ok()?;
                (index > 0).then_some(Self::Goto { index })
            }
            "q" => Some(Self::Quit),
            "n" => {
                let count = parts
                    .next()
                    .and_then(|part| part.parse::<u64>().ok())
                    .unwrap_or(1);
                Some(Self::SearchNext {
                    count: count.max(1),
                })
            }
            "p" => {
                let count = parts
                    .next()
                    .and_then(|part| part.parse::<u64>().ok())
                    .unwrap_or(1);
                Some(Self::SearchPrev {
                    count: count.max(1),
                })
            }
            _ => None,
        }
    }
}

fn parse_range_spec(raw: &str) -> Option<(usize, usize)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let first = parts.next()?;
    let second = parts.next();
    let third = parts.next();

    if let Some(second) = second {
        if third.is_some() {
            return None;
        }
        let start = first.parse::<usize>().ok()?;
        let end = second.parse::<usize>().ok()?;
        if start == 0 || end == 0 {
            return None;
        }
        return Some((start, end));
    }

    if let Some((left, right)) = first.split_once('-') {
        let start = left.trim().parse::<usize>().ok()?;
        let end = right.trim().parse::<usize>().ok()?;
        if start == 0 || end == 0 {
            return None;
        }
        return Some((start, end));
    }

    None
}

fn parse_tail_spec(raw: &str) -> Option<TailSpec> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let lowercase = raw.to_ascii_lowercase();
    if let Some(value) = lowercase.strip_suffix('l') {
        let lines = value.trim().parse::<usize>().ok()?;
        return Some(TailSpec::Lines(lines));
    }

    let (number, multiplier) = match lowercase.chars().last() {
        Some('k') => (lowercase.trim_end_matches('k'), 1024u64),
        Some('m') => (lowercase.trim_end_matches('m'), 1024u64 * 1024),
        Some('b') => (lowercase.trim_end_matches('b'), 1u64),
        _ => (&*lowercase, 0u64),
    };

    if multiplier != 0 {
        let bytes = number.trim().parse::<u64>().ok()?;
        return Some(TailSpec::Bytes(bytes.saturating_mul(multiplier)));
    }

    // Default: interpret as line count.
    let lines = lowercase.parse::<usize>().ok()?;
    Some(TailSpec::Lines(lines))
}

fn parse_seek_spec(raw: &str) -> Option<SeekSpec> {
    let raw = raw.trim().trim_start_matches('@');
    if raw.is_empty() {
        return None;
    }

    let lowercase = raw.to_ascii_lowercase();
    if let Some(rest) = lowercase.strip_prefix("lines") {
        let line = parse_line_number(rest)?;
        return Some(SeekSpec::Line(line));
    }
    if let Some(rest) = lowercase.strip_prefix("line") {
        let line = parse_line_number(rest)?;
        return Some(SeekSpec::Line(line));
    }
    if let Some(value) = lowercase.strip_suffix('%') {
        let percent = value.trim().parse::<u64>().ok()?;
        return Some(SeekSpec::Percent(percent.min(100)));
    }

    let (number, multiplier) = match lowercase.chars().last() {
        Some('k') => (lowercase.trim_end_matches('k'), 1024u64),
        Some('m') => (lowercase.trim_end_matches('m'), 1024u64 * 1024),
        Some('b') => (lowercase.trim_end_matches('b'), 1u64),
        _ => (&*lowercase, 1u64),
    };

    let bytes = number.trim().parse::<u64>().ok()?;
    Some(SeekSpec::Offset(bytes.saturating_mul(multiplier)))
}

fn parse_line_number(raw: &str) -> Option<usize> {
    let trimmed = raw.trim_start_matches(|ch: char| ch.is_whitespace() || ch == ':');
    let value = trimmed.trim();
    if value.is_empty() {
        return None;
    }
    let line = value.parse::<usize>().ok()?;
    if line == 0 {
        return None;
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_previous_goto_and_bare_matches_commands() {
        let prev = PagerCommand::parse(":p").expect("parse :p");
        match prev {
            PagerCommand::SearchPrev { count } => assert_eq!(count, 1),
            other => panic!("expected SearchPrev, got {other:?}"),
        }

        let goto = PagerCommand::parse(":goto 3").expect("parse :goto");
        match goto {
            PagerCommand::Goto { index } => assert_eq!(index, 3),
            other => panic!("expected Goto, got {other:?}"),
        }

        let matches = PagerCommand::parse(":matches").expect("parse bare :matches");
        match matches {
            PagerCommand::Matches { spec } => {
                assert!(spec.pattern.is_none());
                assert_eq!(spec.limit, DEFAULT_MATCH_LIMIT);
                assert_eq!(spec.context, DEFAULT_MATCH_CONTEXT);
            }
            other => panic!("expected Matches, got {other:?}"),
        }
    }

    #[test]
    fn match_spec_accepts_all_limit_and_context() {
        let spec = parse_match_spec(" -i -n all -C 2 foo").expect("match spec");
        assert_eq!(spec.limit, MAX_MATCH_LIMIT);
        assert_eq!(spec.context, 2);
        let pattern = spec.pattern.expect("pattern");
        assert!(pattern.case_insensitive_ascii);
        assert_eq!(pattern.pattern, "foo");
    }

    #[test]
    fn hit_spec_clamps_count_and_context() {
        let spec = parse_hit_spec("-C 999 -n 9999 bar").expect("hit spec");
        assert_eq!(spec.context, MAX_MATCH_CONTEXT);
        assert_eq!(spec.count, MAX_MATCH_LIMIT as u64);
        assert_eq!(spec.pattern.pattern, "bar");
    }

    #[test]
    fn range_spec_rejects_extra_tokens() {
        assert!(parse_range_spec("1 2 3").is_none());
    }
}
