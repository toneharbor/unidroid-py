from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Optional, Union

_DEFAULT_VALUES: dict[str, Any] = {
    "Z": False,
    "B": 0,
    "C": 0,
    "S": 0,
    "I": 0,
    "J": 0,
    "F": 0.0,
    "D": 0.0,
    "V": None,
    "L": 0,
    "[": 0,
}


class JniOp(str, Enum):
    CALL = "call"
    GET = "get"
    SET = "set"
    NEW = "new"


class JniTypeCode(str, Enum):
    BOOLEAN = "Z"
    BYTE = "B"
    CHAR = "C"
    SHORT = "S"
    INT = "I"
    LONG = "J"
    FLOAT = "F"
    DOUBLE = "D"
    VOID = "V"
    OBJECT = "L"
    ARRAY = "["


def default_value(type_code: str) -> Any:
    return _DEFAULT_VALUES.get(type_code, None)


@dataclass
class JniCall:
    op: str
    is_static: bool
    type_code: str
    class_name: str
    name: str
    signature: str
    receiver: int
    args: list[Any] = field(default_factory=list)

    @property
    def op_enum(self) -> Optional[JniOp]:
        try:
            return JniOp(self.op)
        except ValueError:
            return None

    @property
    def type_enum(self) -> Optional[JniTypeCode]:
        try:
            return JniTypeCode(self.type_code)
        except ValueError:
            return None

    @property
    def is_call(self) -> bool:
        return self.op == "call"

    @property
    def is_get(self) -> bool:
        return self.op == "get"

    @property
    def is_set(self) -> bool:
        return self.op == "set"

    @property
    def is_new(self) -> bool:
        return self.op == "new"

    @property
    def is_object_type(self) -> bool:
        return self.type_code in ("L", "[")

    @property
    def is_primitive_type(self) -> bool:
        return self.type_code not in ("L", "[", "V")

    @property
    def default_value(self) -> Any:
        return default_value(self.type_code)

    @classmethod
    def from_dict(cls, d: dict) -> JniCall:
        return cls(
            op=d.get("op", ""),
            is_static=d.get("is_static", False),
            type_code=d.get("type_code", "V"),
            class_name=d.get("class", ""),
            name=d.get("name", ""),
            signature=d.get("signature", ""),
            receiver=d.get("receiver", 0),
            args=list(d.get("args", [])),
        )

    @classmethod
    def _coerce(cls, value: Union[dict, "JniCall"]) -> "JniCall":
        if isinstance(value, cls):
            return value
        if isinstance(value, dict):
            return cls.from_dict(value)
        raise TypeError(f"Expected dict or JniCall, got {type(value).__name__}")


@dataclass
class ImportCall:
    name: str
    args: list[int] = field(default_factory=list)
    original: Optional[int] = None
    return_address: int = 0

    @classmethod
    def from_dict(cls, d: dict) -> ImportCall:
        return cls(
            name=d.get("name", ""),
            args=list(d.get("args", [])),
            original=d.get("original"),
            return_address=d.get("return_address", 0),
        )

    @classmethod
    def _coerce(cls, value: Union[dict, "ImportCall"]) -> "ImportCall":
        if isinstance(value, cls):
            return value
        if isinstance(value, dict):
            return cls.from_dict(value)
        raise TypeError(f"Expected dict or ImportCall, got {type(value).__name__}")


@dataclass
class StopCall:
    reason: str
    address: int
    thread: int
    watch: Optional[dict] = None

    @classmethod
    def from_dict(cls, d: dict) -> StopCall:
        return cls(
            reason=d.get("reason", ""),
            address=d.get("address", 0),
            thread=d.get("thread", 0),
            watch=d.get("watch"),
        )

    @classmethod
    def _coerce(cls, value: Union[dict, "StopCall"]) -> "StopCall":
        if isinstance(value, cls):
            return value
        if isinstance(value, dict):
            return cls.from_dict(value)
        raise TypeError(f"Expected dict or StopCall, got {type(value).__name__}")


class JniHandler:
    """Base class for JNI handlers.

    Subclass and override only the methods you need.  All callbacks
    receive **typed dataclass** arguments (JniCall, ImportCall, StopCall)
    — never raw dicts.

    The ``member`` method dispatches to fine-grained callbacks based on
    ``op`` and ``is_static``.  Override the specific callback instead of
      ``member`` whenever possible — it is much more readable.

    Dispatch tree::

        member()
          +-- on_call_method()        op == "call"
          |     +-- on_call_static_method()    is_static == True
          |     +-- on_call_instance_method()  is_static == False
          +-- on_get_field()         op == "get"
          |     +-- on_get_static_field()      is_static == True
          |     +-- on_get_instance_field()    is_static == False
          +-- on_set_field()         op == "set"
          |     +-- on_set_static_field()      is_static == True
          |     +-- on_set_instance_field()    is_static == False
          +-- on_new_object()        op == "new"
    """

    def on_import(self, emu, call: ImportCall) -> Optional[int]:
        return None

    def stopped(self, emu, stop: StopCall) -> str:
        return "Continue"

    def is_assignable(self, emu, from_class: str, to_class: str) -> Optional[bool]:
        return None

    def member(self, emu, call: JniCall) -> Any:
        return self._dispatch_member(emu, call)

    def _dispatch_member(self, emu, call: JniCall) -> Any:
        op = call.op
        if op == "call":
            return self.on_call_method(emu, call)
        if op == "get":
            return self.on_get_field(emu, call)
        if op == "set":
            return self.on_set_field(emu, call)
        if op == "new":
            return self.on_new_object(emu, call)
        return call.default_value

    def on_call_method(self, emu, call: JniCall) -> Any:
        if call.is_static:
            return self.on_call_static_method(emu, call)
        return self.on_call_instance_method(emu, call)

    def on_call_static_method(self, emu, call: JniCall) -> Any:
        return call.default_value

    def on_call_instance_method(self, emu, call: JniCall) -> Any:
        return call.default_value

    def on_get_field(self, emu, call: JniCall) -> Any:
        if call.is_static:
            return self.on_get_static_field(emu, call)
        return self.on_get_instance_field(emu, call)

    def on_get_static_field(self, emu, call: JniCall) -> Any:
        return call.default_value

    def on_get_instance_field(self, emu, call: JniCall) -> Any:
        return call.default_value

    def on_set_field(self, emu, call: JniCall) -> Any:
        if call.is_static:
            return self.on_set_static_field(emu, call)
        return self.on_set_instance_field(emu, call)

    def on_set_static_field(self, emu, call: JniCall) -> Any:
        return call.default_value

    def on_set_instance_field(self, emu, call: JniCall) -> Any:
        return call.default_value

    def on_new_object(self, emu, call: JniCall) -> Any:
        return call.default_value


class DefaultHandler(JniHandler):
    """Handler that returns default zero values for all JNI calls.

    Equivalent to 0 / 0.0 / None / False depending on type_code.
    Use as-is or subclass to override specific callbacks::

        class MyHandler(DefaultHandler):
            def on_call_static_method(self, emu, call):
                if call.class_name == "com/example/Foo" and call.name == "bar":
                    return 42
                return call.default_value
    """
    pass


class SimpleHandler(JniHandler):
    """Register callbacks per class+method with decorators.

    Usage::

        handler = SimpleHandler()

        @handler.on("com/example/Foo", "bar")
        def handle_bar(emu, call):
            return 42

        @handler.on("com/example/Foo", "getField", op="get")
        def handle_get(emu, call):
            return 0

        # imperative style:
        handler.on("com/example/Foo", "baz", lambda emu, call: "hello")
    """

    _methods: dict[str, Any]

    def __init__(self) -> None:
        self._methods = {}

    def on(self, class_name: str, name: str, func=None, *, op: Optional[str] = None):
        """Register a handler for a specific class+method pair.

        Args:
            class_name: JNI class name
            name: Member name
            func: Handler function ``(emu, call) -> value``, or None for decorator usage
            op: Optional op filter ("call", "get", "set", "new")

        Returns:
            Decorator when func is None, otherwise the function itself.
        """
        def decorator(f):
            key = (f"{op}:" if op else "") + f"{class_name}#{name}"
            self._methods[key] = f
            return f

        if func is not None:
            key = (f"{op}:" if op else "") + f"{class_name}#{name}"
            self._methods[key] = func
            return func
        return decorator

    on_method = on

    def _dispatch_member(self, emu, call: JniCall) -> Any:
        key = f"{call.op}:{call.class_name}#{call.name}"
        if key in self._methods:
            return self._methods[key](emu, call)
        key = f"{call.class_name}#{call.name}"
        if key in self._methods:
            return self._methods[key](emu, call)
        return call.default_value


def _wrap_handler(handler):
    """Wrap a handler for the Rust backend.

    - If *handler* is a ``JniHandler`` subclass instance, returns a thin
      adapter that converts raw dicts to dataclasses before delegating.
    - Otherwise returns *handler* unchanged (passthrough for raw handlers).

    This is called automatically by the ``Emulator`` wrapper — you
    normally do NOT need to call this yourself.
    """
    if isinstance(handler, JniHandler):
        return _wrap_typed_handler(handler)
    return handler


def _wrap_typed_handler(handler: JniHandler):
    """Create a raw handler adapter that converts dicts → dataclasses.

    The adapter receives raw dicts from the Rust backend, converts them
    to typed dataclasses via ``_coerce``, then delegates to the user's
    ``JniHandler`` methods.  This guarantees that user code ALWAYS
    receives dataclasses, regardless of how they override methods.
    """

    class _Wrapped:
        def on_import(self, emu, call_dict):
            call = ImportCall._coerce(call_dict)
            return handler.on_import(emu, call)

        def stopped(self, emu, stop_dict):
            stop = StopCall._coerce(stop_dict)
            return handler.stopped(emu, stop)

        def is_assignable(self, emu, from_class, to_class):
            return handler.is_assignable(emu, from_class, to_class)

        def member(self, emu, call_dict):
            call = JniCall._coerce(call_dict)
            return handler.member(emu, call)

    return _Wrapped()