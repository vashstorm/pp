//! 单个会话的共享值表。每个 JSON 值只定义一次，记录引用已有的值。
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    io::{self, BufRead},
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum Node {
    #[serde(rename = "s")]
    Scalar(Value),
    #[serde(rename = "a")]
    Array(Vec<usize>),
    #[serde(rename = "o")]
    Object(std::collections::BTreeMap<String, usize>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    values: Vec<Node>,
    root: usize,
}

#[derive(Default)]
pub struct Storage {
    nodes: Vec<Node>,
    index: HashMap<u64, Vec<usize>>,
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid session record")
}

impl Storage {
    fn key(node: &Node) -> u64 {
        let mut hash = DefaultHasher::new();
        node.hash(&mut hash);
        hash.finish()
    }

    fn insert(&mut self, node: Node) -> usize {
        let hash = Self::key(&node);
        if let Some(ids) = self.index.get(&hash) {
            for &id in ids {
                if self.nodes[id] == node {
                    return id;
                }
            }
        }
        let id = self.nodes.len();
        self.nodes.push(node);
        self.index.entry(hash).or_default().push(id);
        id
    }

    fn intern(&mut self, value: &Value) -> usize {
        let node = match value {
            Value::Array(values) => Node::Array(values.iter().map(|v| self.intern(v)).collect()),
            Value::Object(values) => Node::Object(
                values
                    .iter()
                    .map(|(k, v)| (k.clone(), self.intern(v)))
                    .collect(),
            ),
            scalar => Node::Scalar(scalar.clone()),
        };
        self.insert(node)
    }

    pub fn encode(&mut self, value: &Value) -> io::Result<Vec<u8>> {
        let first = self.nodes.len();
        let root = self.intern(value);
        serde_json::to_vec(&Record {
            values: self.nodes[first..].to_vec(),
            root,
        })
        .map_err(io::Error::other)
    }

    fn expand(&self, id: usize, depth: usize) -> io::Result<Value> {
        if depth > 128 {
            return Err(invalid());
        }
        Ok(match self.nodes.get(id).ok_or_else(invalid)? {
            Node::Scalar(value) => value.clone(),
            Node::Array(ids) => Value::Array(
                ids.iter()
                    .map(|&id| self.expand(id, depth + 1))
                    .collect::<io::Result<_>>()?,
            ),
            Node::Object(ids) => Value::Object(
                ids.iter()
                    .map(|(key, &id)| Ok((key.clone(), self.expand(id, depth + 1)?)))
                    .collect::<io::Result<Map<_, _>>>()?,
            ),
        })
    }

    pub fn decode(&mut self, line: &[u8]) -> io::Result<Value> {
        let record: Record = serde_json::from_slice(line).map_err(|_| invalid())?;
        let first = self.nodes.len();
        let result = (|| {
            for node in record.values {
                let valid = match &node {
                    Node::Scalar(v) => !v.is_array() && !v.is_object(),
                    Node::Array(ids) => ids.iter().all(|&id| id < self.nodes.len()),
                    Node::Object(ids) => ids.values().all(|&id| id < self.nodes.len()),
                };
                if !valid {
                    return Err(invalid());
                }
                let count = self.nodes.len();
                if self.insert(node) != count {
                    return Err(invalid());
                }
            }
            self.expand(record.root, 0)
        })();
        if result.is_err() {
            self.truncate(first);
        }
        result
    }

    fn truncate(&mut self, count: usize) {
        self.nodes.truncate(count);
        self.index.retain(|_, ids| {
            ids.retain(|&id| id < count);
            !ids.is_empty()
        });
    }
}

/// 返回物理字节数，调用方的文件游标不受解码后的行长度影响。
pub struct Reader<R> {
    reader: R,
    storage: Storage,
    header: bool,
    offset: u64,
}

impl<R: BufRead> Reader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            storage: Storage::default(),
            header: false,
            offset: 0,
        }
    }

    pub fn read_record(&mut self, line: &mut Vec<u8>) -> io::Result<usize> {
        line.clear();
        let count = self.reader.read_until(b'\n', line)?;
        self.offset += count as u64;
        if count == 0 || !line.ends_with(b"\n") {
            return Ok(count);
        }
        if !self.header {
            let value: Value = serde_json::from_slice(line).map_err(|_| invalid())?;
            if value["type"] == "checkpoint" || value.get("version").is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsupported session format",
                ));
            }
            if value["type"] != "session" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "first record must be a session header",
                ));
            }
            self.header = true;
        } else {
            let value = self.storage.decode(line)?;
            *line = serde_json::to_vec(&value).map_err(io::Error::other)?;
            line.push(b'\n');
        }
        Ok(count)
    }

    pub fn into_storage(self) -> Storage {
        self.storage
    }
    pub fn position(&self) -> u64 {
        self.offset
    }
}

impl<R: BufRead + io::Seek> Reader<R> {
    pub fn rewind(&mut self) -> io::Result<()> {
        self.reader.rewind()?;
        self.storage = Storage::default();
        self.header = false;
        self.offset = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shared_values_preserve_records_and_raw_strings() {
        let text = "中文内容".repeat(100);
        let raw = "{ \"x\": 1.00, \"x\":2 }";
        let records = [
            json!({"id":"one", "native":{"text":text,"arguments":raw}}),
            json!({"id":"two","display":{"text":text,"arguments":raw},"history":[{"text":text,"arguments":raw}]}),
        ];
        let mut writer = Storage::default();
        let mut reader = Storage::default();
        let mut bytes = Vec::new();
        for value in records {
            let line = writer.encode(&value).unwrap();
            assert_eq!(reader.decode(&line).unwrap(), value);
            bytes.extend(line);
        }
        assert_eq!(String::from_utf8(bytes).unwrap().matches(&text).count(), 1);
        let next = json!({"native":{"text":text,"arguments":raw}});
        assert_eq!(reader.decode(&writer.encode(&next).unwrap()).unwrap(), next);
    }

    #[test]
    fn rejects_forward_duplicate_and_unknown_records() {
        for line in [
            r#"{"values":[{"a":[0]}],"root":0}"#,
            r#"{"values":[{"s":1},{"s":1}],"root":1}"#,
            r#"{"values":[],"root":0,"version":1}"#,
        ] {
            assert!(Storage::default().decode(line.as_bytes()).is_err());
        }
    }
}
