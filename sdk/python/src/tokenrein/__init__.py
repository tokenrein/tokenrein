"""Tokenrein: runtime spend guardrails for AI agents."""

from . import guard
from .guard import flush
from ._run import Run, current_run
from .errors import (
    BudgetExceeded,
    LocalCapExceeded,
    RunKilled,
    RunPaused,
    RunStopped,
    TokenreinError,
)

__version__ = "0.1.0"

__all__ = [
    "guard",
    "flush",
    "Run",
    "current_run",
    "TokenreinError",
    "BudgetExceeded",
    "LocalCapExceeded",
    "RunStopped",
    "RunPaused",
    "RunKilled",
    "__version__",
]
