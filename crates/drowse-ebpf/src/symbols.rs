//! Kernel-symbol resolution for timer and workqueue callback addresses.
//!
//! `/proc/kallsyms` is parsed once at load into an address-sorted table; a
//! callback address resolves to the greatest symbol at or below it. Addresses
//! read as 0 when `kptr_restrict` hides them (unprivileged); loading needs root
//! here, so we see real addresses.

/// Address-sorted kernel text symbols.
pub struct KallsymsIndex {
    syms: Vec<(u64, String)>,
}

impl KallsymsIndex {
    pub fn load() -> Self {
        let mut syms = Vec::new();
        if let Ok(content) = std::fs::read_to_string("/proc/kallsyms") {
            for line in content.lines() {
                // "ffffffff8109abcd t some_func [module]"
                let mut it = line.split_whitespace();
                let (Some(addr), Some(ty), Some(name)) = (it.next(), it.next(), it.next()) else {
                    continue;
                };
                // Text/weak symbols only; data symbols are not callback targets.
                let t = ty.as_bytes()[0].to_ascii_lowercase();
                if t != b't' && t != b'w' {
                    continue;
                }
                if let Ok(a) = u64::from_str_radix(addr, 16)
                    && a != 0
                {
                    syms.push((a, name.to_string()));
                }
            }
        }
        syms.sort_unstable_by_key(|(a, _)| *a);
        syms.dedup_by_key(|(a, _)| *a);
        KallsymsIndex { syms }
    }

    /// Resolve an address to the enclosing symbol name.
    pub fn resolve(&self, addr: u64) -> Option<String> {
        if self.syms.is_empty() || addr == 0 {
            return None;
        }
        // Greatest symbol with address <= addr.
        let idx = match self.syms.binary_search_by_key(&addr, |(a, _)| *a) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        Some(self.syms[idx].1.clone())
    }
}
