const MAX_NAME_BYTES: usize = 128;

/// Construct the stable public name for an upstream tool.
pub fn public_name(prefix: &str, tool: &str) -> String {
    let stem = format!("{prefix}__");
    if stem.len() + tool.len() <= MAX_NAME_BYTES {
        return stem + tool;
    }

    let hash = fnv1a(tool.as_bytes()) & 0xffff;
    let suffix = format!("{hash:04x}");
    let available = MAX_NAME_BYTES - stem.len() - suffix.len();
    let mut end = available.min(tool.len());
    while !tool.is_char_boundary(end) {
        end -= 1;
    }
    format!("{stem}{}{suffix}", &tool[..end])
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_and_exact_boundary() {
        assert_eq!(public_name("git", "status"), "git__status");
        let tool = "x".repeat(128 - "git__".len());
        assert_eq!(public_name("git", &tool), format!("git__{tool}"));
    }

    #[test]
    fn overflow_is_bounded_and_hashed() {
        let name = public_name("server", &"a".repeat(200));
        assert_eq!(name.len(), 128);
        assert_eq!(&name[name.len() - 4..], "052d");
    }

    #[test]
    fn truncates_only_at_unicode_boundaries() {
        let name = public_name("x", &"é".repeat(100));
        assert_eq!(name.len(), 127); // a UTF-8 scalar cannot fill the final odd byte
        assert!(name.is_char_boundary(name.len() - 4));
    }

    #[test]
    fn hash_is_deterministic_and_distinguishes_truncated_inputs() {
        let prefix = "z".repeat(32);
        let common = "q".repeat(200);
        assert_eq!(public_name(&prefix, &common), public_name(&prefix, &common));
        assert_ne!(
            public_name(&prefix, &(common.clone() + "a")),
            public_name(&prefix, &(common + "b"))
        );
    }
}
