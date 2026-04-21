# Internals

Both Rust Futures and Python Coroutines are poll-based FSMs, which can be adapted to each other.

The key challenge is to avoid busy polling:
- in Rust each poll is provided a waker object, which when triggered causes another poll
- in Python when a future does not want an immediate re-poll, it returns a future object, to which you can attach a callback.

## Rust Future run as a Python Coroutine

### Polling a Coroutine

When a Rust Future is driven by Python event loop, each pymethod call to `.send(value)` results in an invocation of Rust `Future::poll`.

If there is an event_loop (`asyncio.get_running_loop`) — a Python future is produced:
- this future is resolved by a waker, that is to be passed inside of a wrapped Rust Future
- this future is returned from a pymethod `.send`, so that the event-loop attends this Coroutine when Rust waker is triggered.

If there is no event_loop — then it is busy-polling, nothing we can do...

## Python Coroutine as a Rust Future

In order to construct a Rust Future from an Awaitable object in Python:
- the method `__await__` is called, it is expected to produce a Coroutine object
- a Coroutine object is then polled until Completion (see below)
- when a Coroutine produces a value:
    - if the Awaitable object is a Python Future (`asyncio.futures.isfuture` returns `True` for that instance) — then the method `set_result`/`set_exception` is called on the Awaitable
    - the outcome of the Coroutine Completion (value or exception) is the output of the Rust Future
- when a Rust Future is dropped before the Completion:
    - the Coroutine's `.close()` method is called
    - if the Awaitable object is a Python Future (`asyncio.futures.isfuture` returns `True` for that instance) — then the method `cancel` is called on the Awaitable

### Polling a Coroutine

When a Python Coroutine is driven by Rust runtime, each call to Rust `Future::poll` results in an invocation of pymethod `.send(value)`.

There may be four outcomes from `.send` invocation:
- `None` — means a repoll is required (`waker.wake_by_ref(); return Poll::Pending` — do not repoll immediately, let the executor attend other activities)
- a Python Future (`asyncio.futures.isfuture` returns `True` for that object):
    - if the returned future has attribute `_asyncio_future_blocking` set into `True`:
        - the waker is packed into a Python Callback which is then registered on that Python Future via `add_done_callback`.
        - the future is stored as what blocks the current coroutine's progress — it is checked whether it is done on the next poll.
        - The Rust Future returns `Poll::Pending`.
- exception of type `StopIteration` is thrown — this is a Completion of the coroutine with a positive outcome (i.e. this is a result, not an exception)
- exception of any other type is thrown — this is a Completion of the coroutine with a negative outcome (i.e. this is an exception, not a result)
