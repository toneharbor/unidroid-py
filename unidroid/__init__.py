from unidroid._unidroid import (
    Emulator as _RawEmulator,
    JniValue as _JniValue,
    Trace,
    ModuleInfo,
    Mapping,
    Frame,
    ReflectedMember,
    BlockCount,
    CryptoConstant,
    Checkpoint,
    Snapshot,
    Allocation,
    HeapObject,
    Field,
    Target,
    Referrer,
    ApkInfo,
)

from unidroid.helpers import (
    DefaultHandler,
    ImportCall,
    JniCall,
    JniHandler,
    JniOp,
    JniTypeCode,
    SimpleHandler,
    StopCall,
    default_value,
    _wrap_handler,
)

_HANDLER_METHODS = frozenset({
    "call_function_with",
    "call_jni_onload",
    "call_static_jni_method",
    "call_jni_method",
    "run_constructors",
})


class Emulator:
    """Pythonic wrapper around the Rust Emulator.

    Automatically wraps ``JniHandler`` subclass instances so you can
    pass them directly without calling ``_wrap_handler`` yourself.

    All other attributes and methods are delegated to the underlying
    Rust ``Emulator`` instance.
    """

    def __init__(self, *args, **kwargs):
        self._emu = _RawEmulator(*args, **kwargs)

    def __getattr__(self, name):
        if name == "_emu":
            raise AttributeError("_emu not initialized")
        attr = getattr(self._emu, name)
        if name in _HANDLER_METHODS and callable(attr):
            def _auto_wrap_handler(*a, **kw):
                if "handler" in kw:
                    kw["handler"] = _wrap_handler(kw["handler"])
                elif a:
                    a = a[:-1] + (_wrap_handler(a[-1]),)
                return attr(*a, **kw)
            _auto_wrap_handler.__name__ = name
            _auto_wrap_handler.__qualname__ = f"Emulator.{name}"
            return _auto_wrap_handler
        return attr

    @property
    def process_name(self):
        return self._emu.process_name

    def take_jni_errors(self):
        return self._emu.take_jni_errors()


JniValue = _JniValue

__all__ = [
    "Allocation",
    "ApkInfo",
    "BlockCount",
    "Checkpoint",
    "CryptoConstant",
    "DefaultHandler",
    "Emulator",
    "Field",
    "Frame",
    "HeapObject",
    "ImportCall",
    "JniCall",
    "JniHandler",
    "JniOp",
    "JniTypeCode",
    "JniValue",
    "Mapping",
    "ModuleInfo",
    "ReflectedMember",
    "Referrer",
    "SimpleHandler",
    "Snapshot",
    "StopCall",
    "Target",
    "Trace",
    "default_value",
]