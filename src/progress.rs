use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Pending,
    Processing,
    Done,
    Error,
}

#[derive(Debug, Clone)]
pub struct FileResult {
    pub path: PathBuf,
    pub old_size: u64,
    pub new_size: u64,
    pub textures_resized: usize,
    pub textures_total: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum ProgressMsg {
    FileStarted { path: PathBuf },
    FileFinished { result: FileResult },
    FileErrored { path: PathBuf, error: String },
    BatchFinished { elapsed_secs: f64 },
}
