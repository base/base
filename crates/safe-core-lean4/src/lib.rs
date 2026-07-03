pub struct Lean4Exporter {
    path: String,
}
impl Lean4Exporter {
    pub fn new(path: &str) -> Self {
        Self { path: path.to_string() }
    }
    pub fn export<T>(&self, _invariants: &T) -> Result<String, String> {
        Ok(self.path.clone())
    }
}
