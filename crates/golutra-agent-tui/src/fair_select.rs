//! 三个高频来源轮流优先：持续就绪的模型流不能饿死键盘或到期帧。

use std::future::{Future, poll_fn};
use std::task::Poll;

pub(crate) enum Ready<A, B, C> {
    First(A),
    Second(B),
    Third(C),
}

pub(crate) async fn next<A: Future, B: Future, C: Future>(
    priority: &mut usize,
    first: A,
    second: B,
    third: C,
) -> Ready<A::Output, B::Output, C::Output> {
    let (mut first, mut second, mut third) = (
        std::pin::pin!(first),
        std::pin::pin!(second),
        std::pin::pin!(third),
    );
    poll_fn(|cx| {
        for offset in 0..3 {
            let slot = (*priority + offset) % 3;
            let ready = match slot {
                0 => first.as_mut().poll(cx).map(Ready::First),
                1 => second.as_mut().poll(cx).map(Ready::Second),
                _ => third.as_mut().poll(cx).map(Ready::Third),
            };
            if ready.is_ready() {
                *priority = (slot + 1) % 3;
                return ready;
            }
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{pending, ready};

    #[tokio::test]
    async fn continuously_ready_sources_rotate_without_starvation() {
        let mut priority = 0;
        for _ in 0..100 {
            assert!(matches!(
                next(&mut priority, ready(()), ready(()), ready(())).await,
                Ready::First(())
            ));
            assert!(matches!(
                next(&mut priority, ready(()), ready(()), ready(())).await,
                Ready::Second(())
            ));
            assert!(matches!(
                next(&mut priority, ready(()), ready(()), ready(())).await,
                Ready::Third(())
            ));
        }
    }

    #[tokio::test]
    async fn idle_sources_do_not_delay_ready_work() {
        let mut priority = 0;
        for _ in 0..100 {
            assert!(matches!(
                next(&mut priority, pending::<()>(), ready(()), pending::<()>()).await,
                Ready::Second(())
            ));
        }
    }
}
