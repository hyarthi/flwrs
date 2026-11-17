//! MQTT topic names and filters (MQTT 5 §4.7, the same in 3.1.1).

/// A topic name to publish to: not empty, no wildcards, no NUL, at most
/// 65 535 bytes.
pub fn check_topic(topic: &str) -> Result<(), String> {
    if topic.is_empty() {
        return Err("a topic can't be empty".into());
    }
    if topic.len() > 65_535 {
        return Err("a topic is at most 65 535 bytes".into());
    }
    if topic.contains(['+', '#']) {
        return Err(format!("{topic:?}: wildcards (+, #) can't be published to"));
    }
    if topic.contains('\0') {
        return Err(format!("{topic:?}: a topic can't contain NUL"));
    }
    Ok(())
}

/// A topic filter: `+` matches one level, `#` (last) the rest.
pub fn check_filter(filter: &str) -> Result<(), String> {
    if filter.is_empty() {
        return Err("a topic filter can't be empty".into());
    }
    if filter.len() > 65_535 || filter.contains('\0') {
        return Err(format!("{filter:?}: not a valid topic filter"));
    }
    let levels: Vec<&str> = filter.split('/').collect();
    for (i, level) in levels.iter().enumerate() {
        let bad_hash = level.contains('#') && (*level != "#" || i != levels.len() - 1);
        let bad_plus = level.contains('+') && *level != "+";
        if bad_hash || bad_plus {
            return Err(format!(
                "{filter:?}: + and # must be a whole level, and # the last one"
            ));
        }
    }
    Ok(())
}

/// Whether `topic` matches `filter`. Topics starting with `$` (e.g.
/// `$SYS/…`) don't match a filter starting with a wildcard.
pub fn matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && filter.starts_with(['+', '#']) {
        return false;
    }
    let mut topic_levels = topic.split('/');
    for level in filter.split('/') {
        match (level, topic_levels.next()) {
            ("#", _) => return true,
            ("+", Some(_)) => {}
            (f, Some(t)) if f == t => {}
            _ => return false,
        }
    }
    topic_levels.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_and_filters_are_checked() {
        assert!(check_topic("a/b").is_ok());
        assert!(check_topic("/").is_ok());
        for bad in ["", "a/+", "a/#", "a\0b"] {
            assert!(check_topic(bad).is_err(), "{bad:?}");
        }
        for ok in ["#", "+", "a/+/c", "a/#", "+/+", "$SYS/#"] {
            assert!(check_filter(ok).is_ok(), "{ok:?}");
        }
        for bad in ["", "a/#/c", "a#", "a/b+", "a/++"] {
            assert!(check_filter(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn filters_match_topics() {
        assert!(matches("a/b", "a/b"));
        assert!(!matches("a/b", "a/b/c"));
        assert!(matches("a/+", "a/b"));
        assert!(!matches("a/+", "a/b/c"));
        assert!(matches("a/+/c", "a/x/c"));
        assert!(matches("a/#", "a"));
        assert!(matches("a/#", "a/b/c"));
        assert!(matches("#", "a/b"));
        assert!(matches("+/b", "/b"));
        assert!(!matches("#", "$SYS/x"));
        assert!(!matches("+/x", "$SYS/x"));
        assert!(matches("$SYS/#", "$SYS/x"));
    }
}
