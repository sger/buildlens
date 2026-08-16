//! Test results, crashes, and the clustering of related failures.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Started,
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub suite: String,
    pub test: String,
    pub status: TestStatus,
    pub duration_seconds: Option<f64>,
    pub message: Option<String>,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrashType {
    FatalError,
    UnexpectedNil,
    Signal,
    Exception,
    Timeout,
    UnknownProcessExit,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestCrash {
    pub test: Option<String>,
    pub crash_type: CrashType,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct TestSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub crashed: usize,
    pub restarted: usize,
    pub total_duration_seconds: Option<f64>,
    pub slowest: Vec<TestResult>,
    pub tests: Vec<TestResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailureCluster {
    pub fingerprint: String,
    pub category: String,
    pub tests: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlakyTestSummary {
    pub suite: String,
    pub test: String,
    pub runs: usize,
    pub passed: usize,
    pub failed: usize,
    pub failure_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestDurationRegression {
    pub suite: String,
    pub test: String,
    pub previous_seconds: f64,
    pub current_seconds: f64,
    pub change_seconds: f64,
}
