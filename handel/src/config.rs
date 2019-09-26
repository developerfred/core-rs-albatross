use std::str::FromStr;
use std::time::Duration;


#[derive(Clone, Debug)]
pub struct Config {
    /// Number of peers contacted during an update at each level
    pub update_count: usize,

    /// Frequency at which updates are sent to peers
    pub update_interval: Duration,

    /// Timeout for levels
    pub timeout: Duration,

    /// How many peers are contacted at each level
    pub peer_count: usize,

}

fn parse_var<T: FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()
        .and_then(|v| v.parse::<T>().ok())
}

impl Default for Config {
    fn default() -> Self {
        Config {
            update_count: parse_var("HANDEL_UPDATE_COUNT").unwrap_or(1),
            update_interval: Duration::from_millis(
                parse_var("HANDEL_UPDATE_INTERVAL").unwrap_or(100)),
            timeout: Duration::from_millis(
                parse_var("HANDEL_TIMEOUT").unwrap_or(500)),
            peer_count: parse_var("HANDEL_PEER_COUNT").unwrap_or(10),
        }
    }
}
