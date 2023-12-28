use std::time::{SystemTime, UNIX_EPOCH};

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}


#[cfg(test)]
mod tests {
    use crate::time_utils::current_time_millis;

    #[test]
    fn test_current_time_millis() {
        assert!(current_time_millis() < u64::MAX)
    }
}