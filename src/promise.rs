#[cfg(feature = "deferred-resolution")]
use crate::qjs;
use crate::{
    Ctx, Error, FromJs, Function, IntoJs, JsFn, Object, Persistent, Result, SafeRef,
    SendWhenParallel, This, Value,
};
use std::{
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll, Waker},
};

/// Future-aware promise
#[cfg_attr(feature = "doc-cfg", doc(cfg(feature = "futures")))]
pub struct Promise<T> {
    state: SafeRef<State<T>>,
}

struct State<T> {
    result: Option<Result<T>>,
    waker: Option<Waker>,
}

impl<T> State<T> {
    fn resolve(&mut self, result: Result<T>) {
        self.result = Some(result);
        if let Some(waker) = self.waker.take() {
            waker.wake()
        }
    }
}

impl<T> Default for State<T> {
    fn default() -> Self {
        Self {
            result: None,
            waker: None,
        }
    }
}

impl<'js, T> FromJs<'js> for Promise<T>
where
    T: FromJs<'js> + SendWhenParallel + 'static,
{
    fn from_js(ctx: Ctx<'js>, value: Value<'js>) -> Result<Self> {
        let obj = Object::from_js(ctx, value)?;
        let then: Function = obj.get("then")?;
        let state = SafeRef::new(State::default());
        let on_ok = JsFn::new("onSuccess", {
            let state = state.clone();
            move |ctx: Ctx<'js>, value: Value<'js>| {
                let mut state = state.lock();
                state.resolve(T::from_js(ctx, value));
            }
        });
        let on_err = JsFn::new("onError", {
            let state = state.clone();
            move |error: Error| {
                let mut state = state.lock();
                state.resolve(Err(error));
            }
        });
        then.call((This(obj), on_ok, on_err))?;
        Ok(Self { state })
    }
}

impl<T> Future for Promise<T> {
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock();
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }
        state.waker = cx.waker().clone().into();
        Poll::Pending
    }
}

/// Wrapper for futures to convert to JS promises
#[cfg_attr(feature = "doc-cfg", doc(cfg(feature = "futures")))]
#[repr(transparent)]
pub struct PromiseJs<T>(pub T);

impl<T> From<T> for PromiseJs<T> {
    fn from(future: T) -> Self {
        Self(future)
    }
}

#[cfg(any(feature = "async-std", feature = "tokio"))]
impl<'js, T> IntoJs<'js> for PromiseJs<T>
where
    T: Future + 'static,
    T::Output: IntoJs<'js> + 'static,
{
    fn into_js(self, ctx: Ctx<'js>) -> Result<Value<'js>> {
        let (promise, then, catch) = ctx.promise()?;

        let then = Persistent::save(ctx, then);
        let catch = Persistent::save(ctx, catch);

        let runtime = unsafe { &ctx.get_opaque().runtime }
            .try_ref()
            .ok_or(Error::Unknown)?;

        let ctx = ctx.ctx;
        let future = self.0;

        crate::async_shim::spawn_local(async move {
            let result = future.await;

            let rt_lock = runtime.inner.lock();
            let ctx = Ctx::from_ptr(ctx);

            match result.into_js(ctx) {
                Ok(value) => {
                    mem::drop(catch);
                    resolve(ctx, then.restore(ctx).unwrap(), value)
                }
                Err(error) => {
                    mem::drop(then);
                    resolve(
                        ctx,
                        catch.restore(ctx).unwrap(),
                        error.into_js(ctx).unwrap(),
                    )
                }
            };

            mem::drop(rt_lock);
        });

        Ok(promise.into_value())
    }
}

#[cfg(not(feature = "deferred-resolution"))]
fn resolve<'js>(_ctx: Ctx<'js>, func: Function<'js>, value: Value<'js>) {
    if let Err(error) = func.call::<_, Value>((value,)) {
        eprintln!("Error when promise resolution: {}", error);
    }
}

#[cfg(feature = "deferred-resolution")]
fn resolve<'js>(ctx: Ctx<'js>, func: Function<'js>, value: Value<'js>) {
    let args = [func.0.as_js_value(), value.as_js_value()];
    unsafe {
        qjs::JS_EnqueueJob(
            ctx.ctx,
            Some(resolution_job),
            args.len() as _,
            args.as_ptr() as _,
        );
    }
    mem::drop(args);
    mem::drop(func);
    mem::drop(value);
}

#[cfg(feature = "deferred-resolution")]
unsafe extern "C" fn resolution_job(
    ctx: *mut qjs::JSContext,
    argc: qjs::c_int,
    argv: *mut qjs::JSValue,
) -> qjs::JSValue {
    let this = qjs::JS_GetGlobalObject(ctx);
    let func = *argv;
    let argv = argv.offset(1);
    let argc = argc - 1;
    qjs::JS_Call(ctx, func, this, argc, argv)
}

#[cfg(all(test, any(feature = "async-std", feature = "tokio")))]
mod test {
    use crate::{async_shim::block_on, *};

    #[test]
    fn async_fn_no_throw() {
        block_on(async {
            async fn mul2(a: i32, b: i32) -> i32 {
                a * b
            }

            let rt = Runtime::new().unwrap();
            let ctx = Context::full(&rt).unwrap();

            rt.spawn_pending_jobs(None);

            let res: Promise<i32> = ctx.with(|ctx| {
                let global = ctx.globals();
                global
                    .set("mul2", JsFn::new("mul2", |a, b| PromiseJs(mul2(a, b))))
                    .unwrap();
                ctx.eval("mul2(2, 3)").unwrap()
            });

            let res = res.await.unwrap();
            assert_eq!(res, 6);
        });
    }

    #[test]
    #[ignore] // TODO:
    fn async_fn_unhandled_promise() {
        block_on(async {
            async fn doit() {}

            let rt = Runtime::new().unwrap();
            let ctx = Context::full(&rt).unwrap();

            rt.spawn_pending_jobs(None);

            ctx.with(|ctx| {
                let global = ctx.globals();
                global
                    .set("doit", JsFn::new("doit", || PromiseJs(doit())))
                    .unwrap();
                let _ = ctx.eval::<Value, _>("doit()").unwrap();
            });
        });
    }

    #[test]
    #[ignore] // TODO:
    fn async_fn_unhandled_promise_future() {
        block_on(async {
            async fn doit() {}

            let rt = Runtime::new().unwrap();
            let ctx = Context::full(&rt).unwrap();

            rt.spawn_pending_jobs(None);

            let _res: Promise<()> = ctx.with(|ctx| {
                let global = ctx.globals();
                global
                    .set("doit", JsFn::new("doit", || PromiseJs(doit())))
                    .unwrap();
                ctx.eval("doit()").unwrap()
            });
        });
    }
}
