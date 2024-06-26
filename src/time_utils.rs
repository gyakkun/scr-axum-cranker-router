use std::time::{SystemTime, UNIX_EPOCH};

/// Like the `System.currentTimeMillis()` in Java
#[inline]
pub(crate) fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}


#[cfg(test)]
mod tests {
    use crate::time_utils::current_time_millis;

    #[test]
    fn test_current_time_millis() {
        assert!(current_time_millis() < i64::MAX)
    }
}