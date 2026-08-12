// This should fail.

#[remoc::rtc::remote]
pub trait ReservedMethod {
    #[serde(rename = "_59")]
    async fn method(&self) -> Result<u32, remoc::rtc::CallError>;
}

#[remoc::rtc::remote]
pub trait ReservedArgument {
    async fn method(&self, #[serde(rename = "_59")] value: u32) -> Result<u32, remoc::rtc::CallError>;
}
