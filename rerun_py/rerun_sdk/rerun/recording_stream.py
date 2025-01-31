from __future__ import annotations

import functools
import inspect
import uuid
from typing import Any, Callable, TypeVar

from rerun import bindings


# ---
# TODO(#3793): defaulting recording_id to authkey should be opt-in
def new_recording(
    application_id: str,
    *,
    recording_id: str | uuid.UUID | None = None,
    make_default: bool = False,
    make_thread_default: bool = False,
    spawn: bool = False,
    default_enabled: bool = True,
) -> RecordingStream:
    """
    Creates a new recording with a user-chosen application id (name) that can be used to log data.

    If you only need a single global recording, [`rerun.init`][] might be simpler.

    !!! Warning
        If you don't specify a `recording_id`, it will default to a random value that is generated once
        at the start of the process.
        That value will be kept around for the whole lifetime of the process, and even inherited by all
        its subprocesses, if any.

        This makes it trivial to log data to the same recording in a multiprocess setup, but it also means
        that the following code will _not_ create two distinct recordings:
        ```
        rr.init("my_app")
        rr.init("my_app")
        ```

        To create distinct recordings from the same process, specify distinct recording IDs:
        ```
        from uuid import uuid4
        rec = rr.new_recording(application_id="test", recording_id=uuid4())
        rec = rr.new_recording(application_id="test", recording_id=uuid4())
        ```

    Parameters
    ----------
    application_id : str
        Your Rerun recordings will be categorized by this application id, so
        try to pick a unique one for each application that uses the Rerun SDK.

        For example, if you have one application doing object detection
        and another doing camera calibration, you could have
        `rerun.init("object_detector")` and `rerun.init("calibrator")`.
    recording_id : Optional[str]
        Set the recording ID that this process is logging to, as a UUIDv4.

        The default recording_id is based on `multiprocessing.current_process().authkey`
        which means that all processes spawned with `multiprocessing`
        will have the same default recording_id.

        If you are not using `multiprocessing` and still want several different Python
        processes to log to the same Rerun instance (and be part of the same recording),
        you will need to manually assign them all the same recording_id.
        Any random UUIDv4 will work, or copy the recording id for the parent process.
    make_default : bool
        If true (_not_ the default), the newly initialized recording will replace the current
        active one (if any) in the global scope.
    make_thread_default : bool
        If true (_not_ the default), the newly initialized recording will replace the current
        active one (if any) in the thread-local scope.
    spawn : bool
        Spawn a Rerun Viewer and stream logging data to it.
        Short for calling `spawn` separately.
        If you don't call this, log events will be buffered indefinitely until
        you call either `connect`, `show`, or `save`
    default_enabled
        Should Rerun logging be on by default?
        Can be overridden with the RERUN env-var, e.g. `RERUN=on` or `RERUN=off`.

    Returns
    -------
    RecordingStream
        A handle to the [`rerun.RecordingStream`][]. Use it to log data to Rerun.

    """

    application_path = None

    # NOTE: It'd be even nicer to do such thing on the Rust-side so that this little trick would
    # only need to be written once and just work for all languages out of the box… unfortunately
    # we lose most of the details of the python part of the backtrace once we go over the bridge.
    #
    # Still, better than nothing!
    try:
        import inspect
        import pathlib

        # We're trying to grab the filesystem path of the example script that called `init()`.
        # The tricky part is that we don't know how many layers are between this script and the
        # original caller, so we have to walk the stack and look for anything that might look like
        # an official Rerun example.

        MAX_FRAMES = 10  # try the first 10 frames, should be more than enough
        FRAME_FILENAME_INDEX = 1  # `FrameInfo` tuple has `filename` at index 1

        stack = inspect.stack()
        for frame in stack[:MAX_FRAMES]:
            filename = frame[FRAME_FILENAME_INDEX]
            path = pathlib.Path(str(filename)).resolve()  # normalize before comparison!
            if "rerun/examples" in str(path):
                application_path = path
    except Exception:
        pass

    if recording_id is not None:
        recording_id = str(recording_id)

    recording = RecordingStream(
        bindings.new_recording(
            application_id=application_id,
            recording_id=recording_id,
            make_default=make_default,
            make_thread_default=make_thread_default,
            application_path=application_path,
            default_enabled=default_enabled,
        )
    )

    if spawn:
        from rerun.sinks import spawn as _spawn

        _spawn(recording=recording)

    return recording


class RecordingStream:
    """
    A RecordingStream is used to send data to Rerun.

    You can instantiate a RecordingStream by calling either [`rerun.init`][] (to create a global
    recording) or [`rerun.new_recording`][] (for more advanced use cases).

    Multithreading
    --------------

    A RecordingStream can safely be copied and sent to other threads.
    You can also set a recording as the global active one for all threads ([`rerun.set_global_data_recording`][])
    or just for the current thread ([`rerun.set_thread_local_data_recording`][]).

    Similarly, the `with` keyword can be used to temporarily set the active recording for the
    current thread, e.g.:
    ```
    with rec:
        rr.log(...)
    ```

    See also: [`rerun.get_data_recording`][], [`rerun.get_global_data_recording`][],
    [`rerun.get_thread_local_data_recording`][].

    Available methods
    -----------------

    Every function in the Rerun SDK that takes an optional RecordingStream as a parameter can also
    be called as a method on RecordingStream itself.

    This includes, but isn't limited to:

    - Metadata-related functions:
        [`rerun.is_enabled`][], [`rerun.get_recording_id`][], …
    - Sink-related functions:
        [`rerun.connect`][], [`rerun.spawn`][], …
    - Time-related functions:
        [`rerun.set_time_seconds`][], [`rerun.set_time_sequence`][], …
    - Log-related functions:
        [`rerun.log`][], [`rerun.log_components`][], …

    For an exhaustive list, see `help(rerun.RecordingStream)`.

    Micro-batching
    --------------

    Micro-batching using both space and time triggers (whichever comes first) is done automatically
    in a dedicated background thread.

    You can configure the frequency of the batches using the following environment variables:

    - `RERUN_FLUSH_TICK_SECS`:
        Flush frequency in seconds (default: `0.05` (50ms)).
    - `RERUN_FLUSH_NUM_BYTES`:
        Flush threshold in bytes (default: `1048576` (1MiB)).
    - `RERUN_FLUSH_NUM_ROWS`:
        Flush threshold in number of rows (default: `18446744073709551615` (u64::MAX)).

    """

    def __init__(self, inner: bindings.PyRecordingStream) -> None:
        self.inner = inner
        self._prev: RecordingStream | None = None

    def __enter__(self):  # type: ignore[no-untyped-def]
        self._prev = set_thread_local_data_recording(self)
        return self

    def __exit__(self, type, value, traceback):  # type: ignore[no-untyped-def]
        self._prev = set_thread_local_data_recording(self._prev)  # type: ignore[arg-type]

    # NOTE: The type is a string because we cannot reference `RecordingStream` yet at this point.
    def to_native(self: RecordingStream | None) -> bindings.PyRecordingStream | None:
        return self.inner if self is not None else None

    def __del__(self):  # type: ignore[no-untyped-def]
        recording = RecordingStream.to_native(self)
        bindings.flush(blocking=False, recording=recording)


def _patch(funcs):  # type: ignore[no-untyped-def]
    """Adds the given functions as methods to the `RecordingStream` class; injects `recording=self` in passing."""
    import functools
    import os

    # If this is a special RERUN_APP_ONLY context (launched via .spawn), we
    # can bypass everything else, which keeps us from monkey patching methods
    # that never get used.
    if os.environ.get("RERUN_APP_ONLY"):
        return

    # NOTE: Python's closures capture by reference… make sure to copy `fn` early.
    def eager_wrap(fn):  # type: ignore[no-untyped-def]
        @functools.wraps(fn)
        def wrapper(self, *args: Any, **kwargs: Any) -> Any:  # type: ignore[no-untyped-def]
            kwargs["recording"] = self
            return fn(*args, **kwargs)

        return wrapper

    for fn in funcs:
        wrapper = eager_wrap(fn)  # type: ignore[no-untyped-call]
        setattr(RecordingStream, fn.__name__, wrapper)


# ---


def is_enabled(
    recording: RecordingStream | None = None,
) -> bool:
    """
    Is this Rerun recording enabled.

    If false, all calls to the recording are ignored.

    The default can be set in [`rerun.init`][], but is otherwise `True`.

    This can be controlled with the environment variable `RERUN` (e.g. `RERUN=on` or `RERUN=off`).

    """
    return bindings.is_enabled(recording=RecordingStream.to_native(recording))  # type: ignore[no-any-return]


def get_application_id(
    recording: RecordingStream | None = None,
) -> str | None:
    """
    Get the application ID that this recording is associated with, if any.

    Parameters
    ----------
    recording:
        Specifies the [`rerun.RecordingStream`][] to use.
        If left unspecified, defaults to the current active data recording, if there is one.
        See also: [`rerun.init`][], [`rerun.set_global_data_recording`][].

    Returns
    -------
    str
        The application ID that this recording is associated with.

    """
    app_id = bindings.get_application_id(recording=RecordingStream.to_native(recording))
    return str(app_id) if app_id is not None else None


def get_recording_id(
    recording: RecordingStream | None = None,
) -> str | None:
    """
    Get the recording ID that this recording is logging to, as a UUIDv4, if any.

    The default recording_id is based on `multiprocessing.current_process().authkey`
    which means that all processes spawned with `multiprocessing`
    will have the same default recording_id.

    If you are not using `multiprocessing` and still want several different Python
    processes to log to the same Rerun instance (and be part of the same recording),
    you will need to manually assign them all the same recording_id.
    Any random UUIDv4 will work, or copy the recording id for the parent process.

    Parameters
    ----------
    recording:
        Specifies the [`rerun.RecordingStream`][] to use.
        If left unspecified, defaults to the current active data recording, if there is one.
        See also: [`rerun.init`][], [`rerun.set_global_data_recording`][].

    Returns
    -------
    str
        The recording ID that this recording is logging to.

    """
    rec_id = bindings.get_recording_id(recording=RecordingStream.to_native(recording))
    return str(rec_id) if rec_id is not None else None


_patch([is_enabled, get_application_id, get_recording_id])  # type: ignore[no-untyped-call]

# ---


def get_data_recording(
    recording: RecordingStream | None = None,
) -> RecordingStream | None:
    """
    Returns the most appropriate recording to log data to, in the current context, if any.

    * If `recording` is specified, returns that one;
    * Otherwise, falls back to the currently active thread-local recording, if there is one;
    * Otherwise, falls back to the currently active global recording, if there is one;
    * Otherwise, returns None.

    Parameters
    ----------
    recording:
        Specifies the [`rerun.RecordingStream`][] to use.
        If left unspecified, defaults to the current active data recording, if there is one.
        See also: [`rerun.init`][], [`rerun.set_global_data_recording`][].

    Returns
    -------
    Optional[RecordingStream]
        The most appropriate recording to log data to, in the current context, if any.

    """
    result = bindings.get_data_recording(recording=recording)
    return RecordingStream(result) if result is not None else None


def get_global_data_recording() -> RecordingStream | None:
    """
    Returns the currently active global recording, if any.

    Returns
    -------
    Optional[RecordingStream]
        The currently active global recording, if any.

    """
    result = bindings.get_global_data_recording()
    return RecordingStream(result) if result is not None else None


def set_global_data_recording(recording: RecordingStream) -> RecordingStream | None:
    """
    Replaces the currently active global recording with the specified one.

    Parameters
    ----------
    recording:
        The newly active global recording.

    """
    result = bindings.set_global_data_recording(RecordingStream.to_native(recording))
    return RecordingStream(result) if result is not None else None


def get_thread_local_data_recording() -> RecordingStream | None:
    """
    Returns the currently active thread-local recording, if any.

    Returns
    -------
    Optional[RecordingStream]
        The currently active thread-local recording, if any.

    """
    result = bindings.get_thread_local_data_recording()
    return RecordingStream(result) if result is not None else None


def set_thread_local_data_recording(recording: RecordingStream) -> RecordingStream | None:
    """
    Replaces the currently active thread-local recording with the specified one.

    Parameters
    ----------
    recording:
        The newly active thread-local recording.

    """
    result = bindings.set_thread_local_data_recording(recording=RecordingStream.to_native(recording))
    return RecordingStream(result) if result is not None else None


_TFunc = TypeVar("_TFunc", bound=Callable[..., Any])


def thread_local_stream(application_id: str) -> Callable[[_TFunc], _TFunc]:
    """
    Create a thread-local recording stream and use it when executing the decorated function.

    This can be helpful for decorating a function that represents a job or a task that you want to
    to produce its own isolated recording.

    Example
    -------
    ```python
    @rr.thread_local_stream("rerun_example_job")
    def job(name: str) -> None:
        rr.save(f"job_{name}.rrd")
        for i in range(5):
            time.sleep(0.2)
            rr.log("hello", rr.TextLog(f"Hello {i) from Job {name}"))

    threading.Thread(target=job, args=("A",)).start()
    threading.Thread(target=job, args=("B",)).start()
    ```
    This will produce 2 separate rrd files, each only containing the logs from the respective threads.

    Parameters
    ----------
    application_id : str
        The application ID that this recording is associated with.

    """

    def decorator(func: _TFunc) -> _TFunc:
        if inspect.isgeneratorfunction(func):  # noqa: F821

            @functools.wraps(func)
            def generator_wrapper(*args: Any, **kwargs: Any) -> Any:
                gen = func(*args, **kwargs)
                try:
                    with new_recording(application_id, recording_id=uuid.uuid4()):
                        value = next(gen)  # Start the generator inside the context
                        while True:
                            value = gen.send((yield value))  # Continue the generator
                except StopIteration:
                    pass
                finally:
                    gen.close()

            return generator_wrapper  # type: ignore[return-value]
        else:

            @functools.wraps(func)
            def wrapper(*args: Any, **kwargs: Any) -> Any:
                with new_recording(application_id, recording_id=uuid.uuid4()):
                    gen = func(*args, **kwargs)
                    return gen

            return wrapper  # type: ignore[return-value]

    return decorator
