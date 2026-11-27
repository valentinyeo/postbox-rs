#![forbid(unsafe_code)]

pub mod fixtures;
pub mod harness;
pub mod http_client;
pub mod mock_deepgram;
pub mod mock_external;
pub mod mock_hub;
pub mod wire;

pub use harness::{
    CommandOutput, E2eWorkspace, IntegrationMode, M8HookRegistry, ProcessRunner, RealSizeInbox,
    RealSizeInboxStats, control_request, serve_control, test_control_transport,
};
pub use http_client::{
    ClientError, HttpResponse, LoopbackHttpClient, ParsedSseFrame, SseClientStream, SseParseError,
    WsClientConn,
};
pub use mock_deepgram::{
    DeepgramRestMode, DeepgramRestReply, MockDeepgramServer, RecordedDeepgramRest,
    RecordedDeepgramWs, WsScriptStep, WsServerAction, WsTrigger,
};
pub use mock_external::{
    GiphyCdnFault, GiphyFault, MockExternalServer, RecordedExternalRequest, UpdateScenario,
};
pub use mock_hub::{
    ArchiveBatchFault, ChatsFault, HubChatRow, HubMessageRow, MediaBehavior, MessagesFault,
    MockHubServer, RecordedHubRequest, SendFault,
};
pub use wire::{
    LoopbackServer, MultipartPart, RawHttpRequest, RawHttpResponse, WireError, WsFrame, WsOpcode,
};
