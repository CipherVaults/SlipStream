use crate::{EngineError, Result};
use slipstream_protocol::validate_http_url;

pub const MAX_WILDCARD_EXPANSION: usize = 10_000;
const MAX_WILDCARD_GROUPS: usize = 32;
const MAX_PATTERN_BYTES: usize = 8192;
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;

/// Expand inclusive, ascending numeric or alphabetic URL ranges.
pub fn expand_wildcard_url(pattern: &str) -> Result<Vec<String>> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(EngineError::InvalidRequest(
            "wildcard pattern is too long".into(),
        ));
    }
    let mut urls = vec![String::new()];
    let mut rest = pattern;

    // Keep a bracketed IPv6 URL host as part of the literal prefix.
    if let Some((_, authority)) = rest.split_once("://") {
        let host = authority.split(['/', '?', '#']).next().unwrap_or_default();
        let host = host.rsplit('@').next().unwrap_or_default();
        if let Some(address) = host.strip_prefix('[').and_then(|h| h.split_once(']')) {
            if address.0.parse::<std::net::Ipv6Addr>().is_ok() {
                let end = rest.find(']').unwrap() + 1;
                urls[0].push_str(&rest[..end]);
                rest = &rest[end..];
            }
        }
    }

    let mut groups = 0usize;
    while let Some(open) = rest.find('[') {
        groups += 1;
        if groups > MAX_WILDCARD_GROUPS {
            return Err(EngineError::InvalidRequest(format!(
                "wildcard exceeds {MAX_WILDCARD_GROUPS} bracket groups"
            )));
        }
        let fixed = &rest[..open];
        let (expression, tail) = rest[open + 1..].split_once(']').ok_or_else(invalid)?;
        if fixed.contains(']') {
            return Err(invalid());
        }
        let values = expand_range(expression, MAX_WILDCARD_EXPANSION / urls.len())?;
        if values.len() == 1 {
            let value = &values[0];
            let added = fixed.len() + value.len();
            let total_bytes = urls.iter().map(String::len).sum::<usize>() + added * urls.len();
            check_size(total_bytes)?;
            for url in &mut urls {
                url.push_str(fixed);
                url.push_str(value);
            }
        } else {
            let prefix_bytes =
                urls.iter().map(String::len).sum::<usize>() + fixed.len() * urls.len();
            let value_bytes = values.iter().map(String::len).sum::<usize>();
            check_size(prefix_bytes * values.len() + value_bytes * urls.len())?;
            urls = urls
                .iter()
                .flat_map(|url| {
                    values
                        .iter()
                        .map(move |value| format!("{url}{fixed}{value}"))
                })
                .collect();
        }
        rest = tail;
    }

    if rest.contains(']') {
        return Err(invalid());
    }
    check_size(urls.iter().map(String::len).sum::<usize>() + rest.len() * urls.len())?;
    for url in &mut urls {
        url.push_str(rest);
        validate_http_url(url)?;
    }
    Ok(urls)
}

fn check_size(bytes: usize) -> Result<()> {
    if bytes > MAX_BATCH_BYTES {
        return Err(EngineError::InvalidRequest(
            "expanded URL batch is too large".into(),
        ));
    }
    Ok(())
}

fn invalid() -> EngineError {
    EngineError::InvalidRequest("invalid wildcard range".into())
}

fn expand_range(expression: &str, limit: usize) -> Result<Vec<String>> {
    let (bounds, step) = match expression.split_once(':') {
        Some((bounds, step)) => {
            let step = parse_number(step)
                .filter(|step| *step > 0)
                .ok_or_else(invalid)?;
            (bounds, step)
        }
        None => (expression, 1),
    };
    let (first, last) = bounds.split_once('-').ok_or_else(invalid)?;
    let alpha = |s: &str| s.len() == 1 && s.as_bytes()[0].is_ascii_alphabetic();
    let letters = alpha(first)
        && alpha(last)
        && first.as_bytes()[0].is_ascii_lowercase() == last.as_bytes()[0].is_ascii_lowercase();
    let width = if (first.starts_with('0') && first.len() > 1)
        || (last.starts_with('0') && last.len() > 1)
    {
        first.len().max(last.len())
    } else {
        0
    };
    let (first, last) = if letters {
        (
            u64::from(first.as_bytes()[0]),
            u64::from(last.as_bytes()[0]),
        )
    } else {
        (
            parse_number(first).ok_or_else(invalid)?,
            parse_number(last).ok_or_else(invalid)?,
        )
    };
    let intervals = last.checked_sub(first).ok_or_else(invalid)? / step;
    if intervals >= limit as u64 {
        return Err(EngineError::InvalidRequest(format!(
            "wildcard expands to more than {MAX_WILDCARD_EXPANSION} URLs"
        )));
    }
    check_size((intervals as usize + 1) * width.max(20))?;
    Ok((0..=intervals)
        .map(|index| {
            let value = first + index * step;
            if letters {
                char::from(value as u8).to_string()
            } else {
                format!("{value:0width$}")
            }
        })
        .collect())
}

fn parse_number(raw: &str) -> Option<u64> {
    raw.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| raw.parse().ok())
        .flatten()
}
