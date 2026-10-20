"""TokenLens: runtime spend guardrails for AI agents."""

from . import guard
from ._run import Run, current_run
from .errors import (
    BudgetExceeded,
    LocalCapExceeded,
    RunKilled,
    RunPaused,
    RunStopped,
    TokenLensError,
)

__version__ = "0.1.0"

__all__ = [
    "guard",
    "Run",
    "current_run",
    "TokenLensError",
    "BudgetExceeded",
    "LocalCapExceeded",
    "RunStopped",
    "RunPaused",
    "RunKilled",
    "__version__",
]
