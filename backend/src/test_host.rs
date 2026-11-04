//! 测试宿主处理 RPC 调度控制消息，向业务断言交付调用与结果
use gateway_plugin_sdk::{
    Frame, FrameError, Message,
    client::{read_frame, write_frame},
};
use tokio::io::DuplexStream;

pub(crate) async fn receive_business_frame(stream: &mut DuplexStream) -> Result<Frame, FrameError> {
    loop {
        let frame = read_frame(stream).await?;
        match &frame.message {
            Message::Suspend { .. } | Message::CallbackConsumed { .. } => {}
            Message::Resume { id, epoch } => {
                write_frame(
                    stream,
                    &Frame::control(Message::Resumed {
                        id: *id,
                        epoch: *epoch,
                    }),
                )
                .await?;
            }
            _ => return Ok(frame),
        }
    }
}
