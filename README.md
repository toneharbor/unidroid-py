# unidroid-py

Python bindings for [unidroid](https://github.com/toneharbor/unidroid) — a low-memory Android emulator that runs arm64/armv7 `.so` files on a computer, powered by Unicorn in Rust.

## Installation

```bash
uv venv --python 3.13
source .venv/bin/activate
uv pip install maturin
maturin develop --release
```

## Quick Start

```python
from unidroid import Emulator, JniValue, DefaultHandler

class MyHandler(DefaultHandler):
    def on_call_static_method(self, emu, call):
        if call.class_name == "com/example/Foo" and call.name == "bar":
            return 42
        return call.default_value

emu = Emulator(rootfs="rootfs", process_name="com.example.app")
emu.load_library("libfoo.so")
emu.call_jni_onload(0, handler=MyHandler())

result = emu.call_static_jni_method(
    module=0,
    class_name="com/example/Foo",
    method_name="bar",
    signature="(I)J",
    args=[JniValue.int(42)],
    handler=MyHandler(),
)
print(result)
```

## Handler API

All JNI callbacks receive **typed dataclass** arguments (`JniCall`, `ImportCall`, `StopCall`), never raw dicts. The `Emulator` wrapper automatically converts handler arguments — you never need to call `_wrap_handler` yourself.

### JniHandler (base class)

Override only the methods you need:

| Method | When called | Return |
|--------|-------------|--------|
| `on_import(emu, call: ImportCall)` | Library import hook | `Optional[int]` |
| `stopped(emu, stop: StopCall)` | Emulator stopped | `str` (`"Continue"` to resume) |
| `is_assignable(emu, from_class, to_class)` | Class assignability check | `Optional[bool]` |
| `member(emu, call: JniCall)` | Any JNI member access | `Any` |

### Dispatch tree

`member()` auto-dispatches to fine-grained callbacks:

```
member()
  ├── on_call_method()              op == "call"
  │     ├── on_call_static_method()       is_static == True
  │     └── on_call_instance_method()     is_static == False
  ├── on_get_field()                op == "get"
  │     ├── on_get_static_field()         is_static == True
  │     └── on_get_instance_field()       is_static == False
  ├── on_set_field()                op == "set"
  │     ├── on_set_static_field()         is_static == True
  │     └── on_set_instance_field()       is_static == False
  └── on_new_object()               op == "new"
```

### DefaultHandler

Returns default zero values (`0`, `0.0`, `None`, `False`) for all JNI calls. Subclass and override specific callbacks:

```python
from unidroid import DefaultHandler

class MyHandler(DefaultHandler):
    def on_call_static_method(self, emu, call):
        if call.class_name == "com/example/Foo" and call.name == "bar":
            return 42
        return call.default_value
```

### SimpleHandler

Register callbacks per class+method with decorators:

```python
from unidroid import SimpleHandler

handler = SimpleHandler()

@handler.on("com/example/Foo", "bar")
def handle_bar(emu, call):
    return 42

@handler.on("com/example/Foo", "getField", op="get")
def handle_get(emu, call):
    return 0
```

### JniCall dataclass

| Field | Type | Description |
|-------|------|-------------|
| `op` | `str` | `"call"`, `"get"`, `"set"`, `"new"` |
| `is_static` | `bool` | Static vs instance |
| `type_code` | `str` | JNI type code (`"I"`, `"J"`, `"L"`, etc.) |
| `class_name` | `str` | JNI class name |
| `name` | `str` | Member name |
| `signature` | `str` | JNI signature |
| `receiver` | `int` | Object handle (0 for static) |
| `args` | `list` | Arguments |
| `default_value` | property | Default zero value for this type_code |

## API Overview

### Emulator Construction

| Method | Description |
|--------|-------------|
| `Emulator(rootfs, apk, process_name, sdk_version, threads, translation_cache_size, fixed_clock, random_seed)` | Create a new emulator instance |

### Library Loading

| Method | Description |
|--------|-------------|
| `load_library(path)` | Load a shared library and run constructors |
| `load_library_without_constructors(path)` | Load without running constructors |
| `load_apk_library(name)` | Load an APK library |
| `load_apk_library_without_constructors(name)` | Load APK library without constructors |
| `run_constructors(module)` | Run constructors for a previously loaded module |

### JNI Operations

| Method | Description |
|--------|-------------|
| `call_jni_onload(module)` | Call `JNI_OnLoad` |
| `call_static_jni_method(module, class_name, method_name, signature, args)` | Call a static JNI method |
| `call_jni_method(module, class_name, method_name, signature, args)` | Call an instance JNI method |
| `registered_natives()` | List registered native methods |

### Object Management

| Method | Description |
|--------|-------------|
| `class(name)` | Create a class handle |
| `new_object(class, value)` | Create a new object |
| `new_string(value)` | Create a Java string |
| `string(handle)` | Get string value |
| `new_byte_array(bytes)` | Create a byte array |
| `byte_array(handle)` | Get byte array value |
| `new_primitive_array(kind, bytes)` | Create a primitive array |
| `primitive_array(handle)` | Get primitive array value |
| `new_object_array(class, values)` | Create an object array |
| `object_array(handle)` | Get object array elements |
| `new_boxed(value)` | Box a `JniValue` |
| `unboxed(handle)` | Unbox to `JniValue` |
| `new_java_map(class, entries)` | Create a Java Map |
| `java_map(handle)` | Get Map entries |
| `new_java_list(class, items)` | Create a Java List |
| `java_list(handle)` | Get List items |
| `release(handle)` | Release an object handle |

### Reflection

| Method | Description |
|--------|-------------|
| `new_reflected_method(class, name, signature, handle)` | Create a reflected method |
| `new_reflected_field(class, name, signature, handle)` | Create a reflected field |
| `reflected_member(handle)` | Get reflected member info |

### Memory

| Method | Description |
|--------|-------------|
| `malloc(size)` | Allocate memory |
| `free(ptr)` | Free memory |
| `read_bytes(address, size)` | Read bytes from memory |
| `write_bytes(address, data)` | Write bytes to memory |
| `new_cstring(value)` | Create a C string in memory |
| `heap_high_water_mark()` | Get heap high water mark |

### Debugging

| Method | Description |
|--------|-------------|
| `modules()` | List loaded modules |
| `module_bytes(module)` | Get module raw bytes |
| `backtrace()` | Get current backtrace |
| `symbolize(address)` | Symbolize an address |
| `location(address)` | Get location string for address |
| `resolve(text)` | Resolve symbol name to address |
| `maps()` | Get memory maps |
| `registers()` | Get CPU registers |
| `register(name)` | Get single register value |
| `set_register(name, value)` | Set register value |
| `coverage()` | Get code coverage |
| `reset_coverage()` | Reset coverage counters |
| `search(pattern)` | Search memory for byte pattern |
| `crypto_constants()` | Get crypto constants |
| `allocation(address)` | Get heap allocation info |
| `object(address, depth)` | Get heap object info |
| `heap()` | Get all heap allocations |
| `reset_heap()` | Reset heap tracking |

### Breakpoints & Watchpoints

| Method | Description |
|--------|-------------|
| `add_breakpoint(address)` | Add a breakpoint |
| `add_conditional_breakpoint(address, condition)` | Add a conditional breakpoint |
| `remove_breakpoint(address)` | Remove a breakpoint |
| `add_watchpoint(address, size, access)` | Add a watchpoint |
| `remove_watchpoint(address)` | Remove a watchpoint |
| `add_tracepoint(address, tracepoint)` | Add a tracepoint |
| `remove_tracepoint(address)` | Remove a tracepoint |
| `tracepoint_hits(address)` | Get tracepoint hit counts |
| `reset_tracepoint_hits()` | Reset all tracepoint hit counts |

### Checkpoint & Snapshot

| Method | Description |
|--------|-------------|
| `checkpoint()` | Create a checkpoint |
| `rewind(checkpoint)` | Rewind to a checkpoint |
| `snapshot()` | Take a snapshot |

### Tracing

| Method | Description |
|--------|-------------|
| `trace()` | Get current trace config |
| `set_trace(trace)` | Set trace config |

### VFS

| Method | Description |
|--------|-------------|
| `add_file(path, bytes)` | Add a file to the virtual filesystem |
| `add_host_file(path, host_path)` | Add a host file to the VFS |
| `remove_file(path)` | Remove a file from the VFS |
| `file(path)` | Read a file from the VFS |

### Hooking

| Method | Description |
|--------|-------------|
| `hook_import(name)` | Hook an imported symbol |
| `find_symbol(module, name)` | Find a symbol address |
| `symbol_alias(name, target)` | Create a symbol alias |

### Error Handling

| Method | Description |
|--------|-------------|
| `take_jni_errors()` | Retrieve and clear accumulated JNI callback errors |

### JniValue Constructors

| Constructor | Type |
|-------------|------|
| `JniValue.object(handle)` | Object handle |
| `JniValue.boolean(v)` | `bool` |
| `JniValue.byte(v)` | `i8` |
| `JniValue.char(v)` | `u16` |
| `JniValue.short(v)` | `i16` |
| `JniValue.int(v)` | `i32` |
| `JniValue.long(v)` | `i64` |
| `JniValue.float_(v)` | `f32` |
| `JniValue.double(v)` | `f64` |
| `JniValue.void()` | Void |

A `JniValue` has `.kind` (returns type name string) and `.value` (returns Python value).

## Thread Safety

The `Emulator` instance is protected by an atomic busy flag. Concurrent access from multiple Python threads will raise `RuntimeError("Emulator is busy")` rather than causing undefined behavior. Under CPython's GIL this is sufficient; under free-threaded Python (PEP 703), avoid sharing an `Emulator` across threads.

## Development

### Setup

```bash
uv venv --python 3.13
source .venv/bin/activate
uv pip install maturin pytest
maturin develop
```

### Run Tests

```bash
cargo test-lib
pytest
```

### Build Wheel

```bash
maturin build --release
```

## Examples

Examples require test data (APK, SO files) that are not included in the repository due to size.

```bash
# 1. Prepare rootfs (from unidroid repo)
git clone https://github.com/toneharbor/unidroid.git ../unidroid

# 2. Place your APK in sources/
mkdir -p sources
cp /path/to/your.apk sources/Read_7.1.3.32_APKPure.apk

# 3. Build & install
source .venv/bin/activate
maturin develop --release

# 4. Run
python examples/demo_metasec_ml.py
python examples/demo_read_apk.py
```

## License

Apache-2.0