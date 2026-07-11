"""Small process timeout helper for external oracle scripts."""

from __future__ import annotations

from dataclasses import dataclass
from multiprocessing import get_context
from queue import Empty
from typing import Any, Callable


@dataclass(frozen=True)
class OracleCallResult:
    status: str
    value: Any = None
    error: str | None = None


def _worker(queue: Any, func: Callable[..., Any], args: tuple[Any, ...]) -> None:
    try:
        queue.put(OracleCallResult("ok", value=func(*args)))
    except BaseException as exc:  # noqa: BLE001 - report oracle failures verbatim.
        queue.put(OracleCallResult("error", error=f"{type(exc).__name__}: {exc}"))


def _multiprocessing_context():
    try:
        return get_context("fork")
    except ValueError:  # pragma: no cover - non-POSIX fallback.
        return get_context()


def run_with_timeout(
    func: Callable[..., Any],
    args: tuple[Any, ...] = (),
    timeout_seconds: float | None = None,
) -> OracleCallResult:
    """Run `func(*args)` in a child process and classify timeout/error/value."""
    if timeout_seconds is None or timeout_seconds <= 0:
        try:
            return OracleCallResult("ok", value=func(*args))
        except BaseException as exc:  # noqa: BLE001 - keep oracle diagnostics.
            return OracleCallResult("error", error=f"{type(exc).__name__}: {exc}")

    ctx = _multiprocessing_context()
    queue = ctx.Queue(maxsize=1)
    process = ctx.Process(target=_worker, args=(queue, func, args))
    process.start()
    process.join(timeout_seconds)
    if process.is_alive():
        process.terminate()
        process.join()
        return OracleCallResult("timeout")

    try:
        return queue.get_nowait()
    except Empty:
        return OracleCallResult("error", error=f"oracle exited with code {process.exitcode}")
