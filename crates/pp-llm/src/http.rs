pub const MAX_INPUT_BYTES: usize = 20 * 1024 * 1024;

pub struct HttpResponse {
    pub bytes: Vec<u8>,
    pub final_url: String,
    pub content_type: String,
}
