//! MQTT topic filters for subscriptions (MQTT 5 §4.7, §4.8.2; the same
//! syntax in 3.1.1), including shared subscriptions.

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

/// A subscription's filter: a topic filter, or a shared subscription
/// `$share/<group>/<filter>` (MQTT 5; many 3.1.1 brokers accept it too).
pub fn check_subscription(filter: &str) -> Result<(), String> {
    match filter.strip_prefix("$share/") {
        Some(rest) => {
            let (group, inner) = rest.split_once('/').ok_or_else(|| {
                format!("{filter:?}: a shared subscription is $share/<group>/<filter>")
            })?;
            if group.is_empty() || group.contains(['+', '#']) {
                return Err(format!(
                    "{filter:?}: the share group must be a name without + or #"
                ));
            }
            check_filter(inner)
        }
        None => check_filter(filter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_shared_subscriptions() {
        for ok in [
            "#",
            "+",
            "a/+/c",
            "a/#",
            "$SYS/#",
            "$share/g/a/+",
            "$share/g/#",
        ] {
            assert!(check_subscription(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "a/#/c",
            "a#",
            "a/b+",
            "$share/g",
            "$share//a",
            "$share/g+/a",
            "$share/g/a/#/b",
        ] {
            assert!(check_subscription(bad).is_err(), "{bad:?}");
        }
    }
}
