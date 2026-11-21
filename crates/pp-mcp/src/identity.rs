//! MCP 服务身份指纹：基于传输目标计算 SHA-256 十六进制摘要，
//! 不包含项目 cwd、超时或只读清单，也不泄露命令参数、环境变量或认证头。

use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;

use super::{ServerConfig, TransportConfig};

impl ServerConfig {
    pub(crate) fn identity(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update_field(b"pp-mcp-server-v1");
        match &self.transport {
            TransportConfig::Stdio(stdio) => {
                hasher.update_field(b"stdio");
                hasher.update_field(stdio.command.as_bytes());
                hasher.update_len(stdio.args.len());
                for arg in &stdio.args {
                    hasher.update_field(arg.as_bytes());
                }
                match &stdio.cwd {
                    Some(cwd) => {
                        hasher.update(b"\x01");
                        hasher.update_field(cwd.as_os_str().as_bytes());
                    }
                    None => hasher.update(b"\x00"),
                }
                hasher.update_len(stdio.env.len());
                for (key, value) in &stdio.env {
                    hasher.update_field(key.as_bytes());
                    hasher.update_field(value.as_bytes());
                }
            }
            TransportConfig::Http(remote) => {
                hasher.update_field(b"http");
                hasher.update_field(remote.url.as_str().as_bytes());
            }
            TransportConfig::Sse(remote) => {
                hasher.update_field(b"sse");
                hasher.update_field(remote.url.as_str().as_bytes());
            }
        }
        hasher.finish_hex()
    }
}

// 零外部依赖的 FIPS 180-4 SHA-256 实现。
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_bytes: u64,
}

impl Sha256 {
    const INIT: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: Self::INIT,
            buffer: [0; 64],
            buffer_len: 0,
            total_bytes: 0,
        }
    }

    fn update_len(&mut self, len: usize) {
        self.update(&(len as u64).to_be_bytes());
    }

    fn update_field(&mut self, bytes: &[u8]) {
        self.update_len(bytes.len());
        self.update(bytes);
    }

    fn update(&mut self, mut input: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(input.len() as u64);
        if self.buffer_len > 0 {
            let take = (64 - self.buffer_len).min(input.len());
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&input[..take]);
            self.buffer_len += take;
            input = &input[take..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffer_len = 0;
            }
        }
        let (chunks, remainder) = input.as_chunks::<64>();
        for block in chunks {
            self.compress(block);
        }
        if !remainder.is_empty() {
            self.buffer[..remainder.len()].copy_from_slice(remainder);
            self.buffer_len = remainder.len();
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, word) in w[..16].iter_mut().enumerate() {
            let off = i * 4;
            *word =
                u32::from_be_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (word, k) in w.into_iter().zip(Self::K) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, delta) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(delta);
        }
    }

    fn finish_hex(mut self) -> String {
        let total_bits = self.total_bytes.wrapping_mul(8);
        self.update(&[0x80]);
        if self.buffer_len > 56 {
            let pad = 64 - self.buffer_len;
            self.update(&[0u8; 64][..pad]);
        }
        let pad = 56 - self.buffer_len;
        self.update(&[0u8; 56][..pad]);
        self.update(&total_bits.to_be_bytes());
        let mut out = String::with_capacity(64);
        for word in self.state {
            let _ = write!(out, "{word:08x}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RemoteConfig, StdioConfig};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    #[test]
    fn sha256_matches_fips_test_vectors() {
        let mut empty = Sha256::new();
        empty.update(b"");
        assert_eq!(
            empty.finish_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut abc = Sha256::new();
        abc.update(b"abc");
        assert_eq!(
            abc.finish_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn server_identity_tracks_target_changes_and_ignores_timeout_and_readonly_list() {
        let base = ServerConfig {
            transport: TransportConfig::Stdio(StdioConfig {
                command: "node".into(),
                args: vec!["server.js".into()],
                env: BTreeMap::from([("MODE".into(), "a".into())]),
                cwd: Some(PathBuf::from("/opt/mcp")),
            }),
            timeout_seconds: 30,
            timeout_millis: Some(30_000),
            read_only_tools: BTreeSet::new(),
        };
        let mut same_target = base.clone();
        same_target.timeout_seconds = 60;
        same_target.timeout_millis = Some(60_000);
        same_target.read_only_tools.insert("read".into());
        assert_eq!(base.identity(), same_target.identity());
        assert_eq!(base.identity().len(), 64);

        let mut other_args = base.clone();
        other_args.transport = TransportConfig::Stdio(StdioConfig {
            command: "node".into(),
            args: vec!["other.js".into()],
            env: BTreeMap::from([("MODE".into(), "a".into())]),
            cwd: Some(PathBuf::from("/opt/mcp")),
        });
        assert_ne!(base.identity(), other_args.identity());

        let http = ServerConfig {
            transport: TransportConfig::Http(RemoteConfig {
                url: reqwest::Url::parse("https://example.com/mcp").unwrap(),
                headers: reqwest::header::HeaderMap::new(),
            }),
            timeout_seconds: 30,
            timeout_millis: None,
            read_only_tools: BTreeSet::new(),
        };
        let sse = ServerConfig {
            transport: TransportConfig::Sse(RemoteConfig {
                url: reqwest::Url::parse("https://example.com/mcp").unwrap(),
                headers: reqwest::header::HeaderMap::new(),
            }),
            timeout_seconds: 30,
            timeout_millis: None,
            read_only_tools: BTreeSet::new(),
        };
        assert_ne!(http.identity(), sse.identity());
        assert_ne!(http.identity(), base.identity());
    }
}
