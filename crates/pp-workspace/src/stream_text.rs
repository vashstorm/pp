//! 增量 UTF-8 流式字节解码器，跨读取边界保留不完整的多字节前缀。

#[derive(Default)]
pub struct StreamText {
    tail: [u8; 4],
    tail_len: usize,
}

impl StreamText {
    pub fn append_delta(&mut self, mut bytes: &[u8]) -> String {
        let mut delta = String::with_capacity(bytes.len() + self.tail_len);
        while self.tail_len > 0 && !bytes.is_empty() {
            let count = (self.tail.len() - self.tail_len).min(bytes.len());
            self.tail[self.tail_len..self.tail_len + count].copy_from_slice(&bytes[..count]);
            self.tail_len += count;
            bytes = &bytes[count..];
            let used = decode_prefix(&self.tail[..self.tail_len], &mut delta);
            self.tail.copy_within(used..self.tail_len, 0);
            self.tail_len -= used;
        }
        if self.tail_len == 0 {
            let used = decode_prefix(bytes, &mut delta);
            let tail = &bytes[used..];
            self.tail[..tail.len()].copy_from_slice(tail);
            self.tail_len = tail.len();
        }
        delta
    }

    pub fn finish_delta(&mut self) -> String {
        let delta = String::from_utf8_lossy(&self.tail[..self.tail_len]).into_owned();
        self.tail_len = 0;
        delta
    }
}

// Decode only complete code points/errors. An incomplete suffix needs the next
// read (or EOF), otherwise split multibyte characters would become replacements.
fn decode_prefix(mut bytes: &[u8], output: &mut String) -> usize {
    let total = bytes.len();
    loop {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                output.push_str(text);
                return total;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                output.push_str(
                    std::str::from_utf8(&bytes[..valid]).expect("validated UTF-8 prefix"),
                );
                bytes = &bytes[valid..];
                match error.error_len() {
                    Some(count) => {
                        output.push(char::REPLACEMENT_CHARACTER);
                        bytes = &bytes[count..];
                    }
                    None => return total - bytes.len(),
                }
            }
        }
    }
}
