use std::fmt::Write;

/// Converts a byte slice to a hex string representation
///
/// # Examples
///
/// ```
/// let bytes = vec![0xde, 0xad, 0xbe, 0xef];
/// let result = bytes_to_string(&bytes);
/// assert_eq!(result, "deadbeef");
/// ```
pub fn bytes_to_string(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut output, b| {
        let _ = write!(output, "{:02x}", b);
        output
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bytes_to_string() {
        let bytes = vec![0xde, 0xad, 0xbe, 0xef];
        let result = bytes_to_string(&bytes);
        assert_eq!(result, "deadbeef");
    }

    #[test]
    fn test_bytes_to_string_empty() {
        let bytes: Vec<u8> = vec![];
        let result = bytes_to_string(&bytes);
        assert_eq!(result, "");
    }
}
