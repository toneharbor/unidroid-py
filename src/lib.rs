//! Python bindings for `unidroid`.
//!
//! # Threading model
//!
//! `Emulator` instances are **not safe for concurrent use**. All method
//! calls on a single `Emulator` instance must be serialized across Python
//! threads. The binding enforces this at runtime via an internal busy flag:
//! if a second thread enters while another is inside a long-running method
//! (e.g. `call_function`, `load_library`), a `RuntimeError` is raised.
//!
//! Long-running methods release the GIL during execution, but the
//! `Emulator` remains exclusively borrowed until the call returns.
//! User-supplied JNI handlers may call back into Python while holding this
//! borrow; they must not invoke other `Emulator` methods on the same
//! instance from inside the handler.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use unidroid_core::{
    Allocation, Checkpoint, Crash, Emulator, Field, Handle, HeapObject, ImportCall, JniCall,
    JniHandler, JniValue, Mapping, Module, ModuleInfo, PrimitiveType, Referrer,
    ReflectedMemberKind, Resume, Snapshot as CoreSnapshot, Stop, StopReason, Target, Trace,
    Tracepoint, Watch, WatchAction,
};

macro_rules! wrap_result {
    ($expr:expr) => {
        match $expr {
            Ok(value) => Ok(value),
            Err(error) => Err(to_py_err(error)),
        }
    };
}

fn py_to_jni_value(type_code: u8, obj: &Bound<'_, PyAny>) -> PyResult<JniValue> {
    match type_code {
        b'L' | b'[' => obj.extract::<u64>().map(|v| JniValue::Object(Handle(v))),
        b'Z' => obj.extract::<bool>().map(JniValue::Boolean),
        b'B' => obj.extract::<i8>().map(JniValue::Byte),
        b'C' => obj.extract::<u16>().map(JniValue::Char),
        b'S' => obj.extract::<i16>().map(JniValue::Short),
        b'I' => obj.extract::<i32>().map(JniValue::Int),
        b'J' => obj.extract::<i64>().map(JniValue::Long),
        b'F' => obj.extract::<f32>().map(JniValue::Float),
        b'D' => obj.extract::<f64>().map(JniValue::Double),
        b'V' => Ok(JniValue::Void),
        _ => Err(PyRuntimeError::new_err(format!(
            "unknown JNI type code: '{}'",
            char::from(type_code)
        ))),
    }
    .map_err(|e| {
        if e.is_instance_of::<PyRuntimeError>(obj.py()) {
            e
        } else {
            PyRuntimeError::new_err(format!(
                "cannot convert Python value to JniValue for type '{}': {:?}",
                char::from(type_code),
                e
            ))
        }
    })
}

fn to_py_err(error: Box<dyn std::error::Error>) -> PyErr {
    PyRuntimeError::new_err(err_to_string(error))
}

fn err_to_string(error: Box<dyn std::error::Error>) -> String {
    if let Some(crash) = error.downcast_ref::<Crash>() {
        format!("{}\n\n{}", crash.message, crash.report())
    } else {
        error.to_string()
    }
}

fn to_panic_err(panic: Box<dyn std::any::Any + Send>) -> PyErr {
    let msg = if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "Rust panic".into()
    };
    PyRuntimeError::new_err(format!("unidroid panic: {msg}"))
}

fn catch_unwind<F, R>(f: F) -> PyResult<R>
where
    F: FnOnce() -> PyResult<R> + panic::UnwindSafe,
{
    match panic::catch_unwind(f) {
        Ok(result) => result,
        Err(panic) => Err(to_panic_err(panic)),
    }
}

#[pyclass(name = "Emulator")]
struct PyEmulator {
    emu: UnsafeCell<Emulator>,
    process_name: String,
    busy: AtomicBool,
    // CHANGED: JNI 回调的 Python 异常统一记入此队列，可通过 `take_jni_errors` 取出。
    last_errors: Mutex<Vec<String>>,
}

unsafe impl Send for PyEmulator {}
unsafe impl Sync for PyEmulator {}

/// CHANGED: RAII 独占 guard。构造时通过 `compare_exchange` 原子占用 `busy`，
/// Drop 时释放。这消除了 `emu()`/`emu_mut()` 的 check-then-act 竞态窗口。
struct EmuGuard<'a> {
    owner: &'a PyEmulator,
    _not_send: PhantomData<*mut ()>,
}

impl<'a> Drop for EmuGuard<'a> {
    fn drop(&mut self) {
        self.owner.busy.store(false, Ordering::Release);
    }
}

impl<'a> Deref for EmuGuard<'a> {
    type Target = Emulator;
    fn deref(&self) -> &Emulator {
        unsafe { &*self.owner.emu.get() }
    }
}

impl<'a> DerefMut for EmuGuard<'a> {
    fn deref_mut(&mut self) -> &mut Emulator {
        unsafe { &mut *self.owner.emu.get() }
    }
}

impl PyEmulator {
    /// CHANGED: 原子占用式访问，返回 RAII guard。
    fn lock_emu(&self) -> PyResult<EmuGuard<'_>> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(PyRuntimeError::new_err(
                "Emulator is busy; concurrent access is not allowed",
            ));
        }
        Ok(EmuGuard {
            owner: self,
            _not_send: PhantomData,
        })
    }

    fn emu(&self) -> PyResult<EmuGuard<'_>> {
        self.lock_emu()
    }

    fn emu_mut(&self) -> PyResult<EmuGuard<'_>> {
        self.lock_emu()
    }

    fn ptr_usize(&self) -> usize {
        self.emu.get() as usize
    }

    /// CHANGED: RAII guard for `py.detach` path. Same atomic logic as `EmuGuard`,
    /// but named separately to make the two access patterns distinct at the type level.
    fn enter_detach(&self) -> PyResult<DetachGuard<'_>> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(PyRuntimeError::new_err(
                "Emulator is already executing in another thread; concurrent access is not allowed",
            ));
        }
        Ok(DetachGuard {
            owner: self,
            _not_send: PhantomData,
        })
    }
}

struct DetachGuard<'a> {
    owner: &'a PyEmulator,
    _not_send: PhantomData<*mut ()>,
}

impl<'a> Drop for DetachGuard<'a> {
    fn drop(&mut self) {
        self.owner.busy.store(false, Ordering::Release);
    }
}

#[pymethods]
impl PyEmulator {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (rootfs=None, apk=None, process_name=None, sdk_version=None, threads=true, translation_cache_size=None, fixed_clock=None, random_seed=None))]
    fn new(
        rootfs: Option<&str>,
        apk: Option<&str>,
        process_name: Option<&str>,
        sdk_version: Option<&str>,
        threads: bool,
        translation_cache_size: Option<u32>,
        fixed_clock: Option<i64>,
        random_seed: Option<u64>,
    ) -> PyResult<Self> {
        catch_unwind(AssertUnwindSafe(|| {
            let mut builder = Emulator::builder();
            if let Some(rootfs) = rootfs {
                builder = builder.rootfs(rootfs);
            }
            if let Some(apk) = apk {
                builder = builder.apk(apk);
            }
            let pname = process_name.unwrap_or("").to_string();
            if let Some(pn) = process_name {
                builder = builder.process_name(pn);
            }
            if let Some(sdk) = sdk_version {
                builder = builder.system_property("ro.build.version.sdk", sdk);
            }
            builder = builder.threads(threads);
            if let Some(size) = translation_cache_size {
                builder = builder.translation_cache_size(size);
            }
            if let Some(clock) = fixed_clock {
                builder = builder.fixed_clock(clock);
            }
            if let Some(seed) = random_seed {
                builder = builder.random_seed(seed);
            }
            let emu = wrap_result!(builder.build())?;
            Ok(PyEmulator {
                emu: UnsafeCell::new(emu),
                process_name: pname,
                busy: AtomicBool::new(false),
                last_errors: Mutex::new(Vec::new()),
            })
        }))
    }

    #[getter]
    fn process_name(&self) -> &str {
        &self.process_name
    }

    /// CHANGED: 获取并清空累积的 JNI 回调错误。
    fn take_jni_errors(&self) -> Vec<String> {
        let mut g = self.last_errors.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *g)
    }

    fn load_library(&self, py: Python<'_>, path: String) -> PyResult<usize> {
        let _guard = self.enter_detach()?;
        let ptr = self.ptr_usize();
        catch_unwind(AssertUnwindSafe(|| {
            let result: Result<usize, String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.load_library(&path).map(|m| m.0).map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn load_library_without_constructors(&self, py: Python<'_>, path: String) -> PyResult<usize> {
        let _guard = self.enter_detach()?;
        let ptr = self.ptr_usize();
        catch_unwind(AssertUnwindSafe(|| {
            let result: Result<usize, String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.load_library_without_constructors(&path)
                    .map(|m| m.0)
                    .map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn load_apk_library(&self, py: Python<'_>, name: String) -> PyResult<usize> {
        let _guard = self.enter_detach()?;
        let ptr = self.ptr_usize();
        catch_unwind(AssertUnwindSafe(|| {
            let result: Result<usize, String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.load_apk_library(&name)
                    .map(|m| m.0)
                    .map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn load_apk_library_without_constructors(
        &self,
        py: Python<'_>,
        name: String,
    ) -> PyResult<usize> {
        let _guard = self.enter_detach()?;
        let ptr = self.ptr_usize();
        catch_unwind(AssertUnwindSafe(|| {
            let result: Result<usize, String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.load_apk_library_without_constructors(&name)
                    .map(|m| m.0)
                    .map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn find_symbol(&self, py: Python<'_>, module: usize, name: String) -> PyResult<u64> {
        let _guard = self.enter_detach()?;
        let ptr = self.ptr_usize();
        catch_unwind(AssertUnwindSafe(|| {
            let result: Result<u64, String> = py.detach(move || {
                let emu = unsafe { &*(ptr as *const Emulator) };
                emu.find_symbol(Module(module), &name)
                    .map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn symbol_alias(&self, name: &str, target: &str) -> PyResult<()> {
        self.emu_mut()?.symbol_alias(name, target);
        Ok(())
    }

    fn hook_import(&self, py: Python<'_>, name: String) -> PyResult<()> {
        let _guard = self.enter_detach()?;
        let ptr = self.ptr_usize();
        catch_unwind(AssertUnwindSafe(|| {
            let result: Result<(), String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.hook_import(&name).map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn call_function(&self, py: Python<'_>, function: u64, args: Vec<u64>) -> PyResult<u64> {
        let _guard = self.enter_detach()?;
        let ptr = self.ptr_usize();
        catch_unwind(AssertUnwindSafe(|| {
            let result: Result<u64, String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.call_function(function, &args).map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn call_function_with(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        function: u64,
        args: Vec<u64>,
        handler: Py<PyAny>,
    ) -> PyResult<u64> {
        let guard_holder = slf.borrow();
        let _guard = guard_holder.enter_detach()?;
        let ptr = guard_holder.ptr_usize();
        let self_ref = slf.clone().unbind();
        catch_unwind(AssertUnwindSafe(|| {
            let mut jni_handler = PythonJniHandler {
                handler,
                py_emu: self_ref,
            };
            let result: Result<u64, String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.call_function_with(&mut jni_handler, function, &args)
                    .map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn call_jni_onload(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        module: usize,
        handler: Py<PyAny>,
    ) -> PyResult<u64> {
        let guard_holder = slf.borrow();
        let _guard = guard_holder.enter_detach()?;
        let ptr = guard_holder.ptr_usize();
        let self_ref = slf.clone().unbind();
        catch_unwind(AssertUnwindSafe(|| {
            let mut jni_handler = PythonJniHandler {
                handler,
                py_emu: self_ref,
            };
            let result: Result<u64, String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.call_jni_onload(Module(module), &mut jni_handler)
                    .map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn call_static_jni_method(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        class: String,
        method: String,
        args: Vec<PyJniValue>,
        handler: Py<PyAny>,
    ) -> PyResult<PyJniValue> {
        let guard_holder = slf.borrow();
        let _guard = guard_holder.enter_detach()?;
        let ptr = guard_holder.ptr_usize();
        let self_ref = slf.clone().unbind();
        catch_unwind(AssertUnwindSafe(|| {
            let jni_args: Vec<JniValue> = args.iter().map(|v| v.0).collect();
            let mut jni_handler = PythonJniHandler {
                handler,
                py_emu: self_ref,
            };
            let result: Result<JniValue, String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.call_static_jni_method(&mut jni_handler, &class, &method, &jni_args)
                    .map_err(err_to_string)
            });
            result.map(PyJniValue).map_err(PyRuntimeError::new_err)
        }))
    }

    fn call_jni_method(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        class: String,
        receiver: u64,
        method: String,
        args: Vec<PyJniValue>,
        handler: Py<PyAny>,
    ) -> PyResult<PyJniValue> {
        let guard_holder = slf.borrow();
        let _guard = guard_holder.enter_detach()?;
        let ptr = guard_holder.ptr_usize();
        let self_ref = slf.clone().unbind();
        catch_unwind(AssertUnwindSafe(|| {
            let jni_args: Vec<JniValue> = args.iter().map(|v| v.0).collect();
            let mut jni_handler = PythonJniHandler {
                handler,
                py_emu: self_ref,
            };
            let result: Result<JniValue, String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.call_jni_method(
                    &mut jni_handler,
                    &class,
                    Handle(receiver),
                    &method,
                    &jni_args,
                )
                .map_err(err_to_string)
            });
            result.map(PyJniValue).map_err(PyRuntimeError::new_err)
        }))
    }

    fn run_constructors(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        module: usize,
        handler: Py<PyAny>,
    ) -> PyResult<()> {
        let guard_holder = slf.borrow();
        let _guard = guard_holder.enter_detach()?;
        let ptr = guard_holder.ptr_usize();
        let self_ref = slf.clone().unbind();
        catch_unwind(AssertUnwindSafe(|| {
            let mut jni_handler = PythonJniHandler {
                handler,
                py_emu: self_ref,
            };
            let result: Result<(), String> = py.detach(move || {
                let emu = unsafe { &mut *(ptr as *mut Emulator) };
                emu.run_constructors(Module(module), &mut jni_handler)
                    .map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn registered_natives(&self) -> PyResult<Vec<(String, String, u64)>> {
        Ok(self.emu()?.registered_natives())
    }

    #[pyo3(signature = (size))]
    fn malloc(&self, size: usize) -> PyResult<u64> {
        wrap_result!(self.emu_mut()?.malloc(size))
    }

    fn free(&self, ptr: u64) -> PyResult<()> {
        self.emu_mut()?.free(ptr);
        Ok(())
    }

    fn read_bytes(&self, address: u64, size: usize) -> PyResult<Vec<u8>> {
        wrap_result!(self.emu()?.read_bytes(address, size))
    }

    fn write_bytes(&self, address: u64, bytes: Vec<u8>) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.write_bytes(address, &bytes))
    }

    fn new_cstring(&self, value: &str) -> PyResult<u64> {
        wrap_result!(self.emu_mut()?.new_cstring(value))
    }

    fn heap_high_water_mark(&self) -> PyResult<u64> {
        Ok(self.emu()?.heap_high_water_mark())
    }

    fn class(&self, name: &str) -> PyResult<u64> {
        Ok(self.emu_mut()?.class(name).0)
    }

    fn superclass(&self, class: &str, superclass: &str) -> PyResult<()> {
        self.emu_mut()?.superclass(class, superclass);
        Ok(())
    }

    /// CHANGED: 单次 `emu_mut()` 占用，避免每个 match 分支重复 CAS。
    fn new_object(&self, class: &str, value: PyJniValue) -> PyResult<u64> {
        let mut guard = self.emu_mut()?;
        Ok(match value.0 {
            JniValue::Object(h) => guard.new_object(class, h.0).0,
            JniValue::Boolean(v) => guard.new_object(class, v).0,
            JniValue::Byte(v) => guard.new_object(class, v).0,
            JniValue::Char(v) => guard.new_object(class, v).0,
            JniValue::Short(v) => guard.new_object(class, v).0,
            JniValue::Int(v) => guard.new_object(class, v).0,
            JniValue::Long(v) => guard.new_object(class, v).0,
            JniValue::Float(v) => guard.new_object(class, v).0,
            JniValue::Double(v) => guard.new_object(class, v).0,
            JniValue::Void => guard.new_object(class, 0i64).0,
        })
    }

    fn new_string(&self, value: &str) -> PyResult<u64> {
        Ok(self.emu_mut()?.new_string(value).0)
    }

    fn string(&self, handle: u64) -> PyResult<String> {
        wrap_result!(self.emu()?.string(Handle(handle)))
    }

    fn new_byte_array(&self, bytes: Vec<u8>) -> PyResult<u64> {
        Ok(self.emu_mut()?.new_byte_array(&bytes).0)
    }

    fn byte_array(&self, handle: u64) -> PyResult<Vec<u8>> {
        wrap_result!(self.emu()?.byte_array(Handle(handle)))
    }

    fn new_primitive_array(&self, kind: &str, bytes: Vec<u8>) -> PyResult<u64> {
        let prim = parse_primitive_type(kind)?;
        Ok(self.emu_mut()?.new_primitive_array(prim, &bytes).0)
    }

    fn primitive_array(&self, handle: u64) -> PyResult<Vec<u8>> {
        wrap_result!(self.emu()?.primitive_array(Handle(handle)))
    }

    fn new_object_array(&self, class: &str, values: Vec<u64>) -> PyResult<u64> {
        let handles: Vec<Handle> = values.iter().map(|&v| Handle(v)).collect();
        wrap_result!(self
            .emu_mut()?
            .new_object_array(class, &handles)
            .map(|h| h.0))
    }

    fn object_array(&self, handle: u64) -> PyResult<Vec<u64>> {
        wrap_result!(self
            .emu_mut()?
            .object_array(Handle(handle))
            .map(|v| v.iter().map(|h| h.0).collect()))
    }

    fn release(&self, handle: u64) -> PyResult<()> {
        self.emu_mut()?.release(Handle(handle));
        Ok(())
    }

    fn new_boxed(&self, value: PyJniValue) -> PyResult<u64> {
        wrap_result!(self.emu_mut()?.new_boxed(value.0).map(|h| h.0))
    }

    fn unboxed(&self, handle: u64) -> PyResult<PyJniValue> {
        wrap_result!(self.emu()?.unboxed(Handle(handle)).map(PyJniValue))
    }

    fn new_java_map(&self, class: &str, entries: Vec<(u64, u64)>) -> PyResult<u64> {
        let pairs: Vec<(Handle, Handle)> = entries
            .iter()
            .map(|&(k, v)| (Handle(k), Handle(v)))
            .collect();
        wrap_result!(self.emu_mut()?.new_java_map(class, &pairs).map(|h| h.0))
    }

    fn java_map(&self, handle: u64) -> PyResult<Vec<(u64, u64)>> {
        wrap_result!(self
            .emu_mut()?
            .java_map(Handle(handle))
            .map(|v| v.iter().map(|(k, h)| (k.0, h.0)).collect()))
    }

    fn new_java_list(&self, class: &str, items: Vec<u64>) -> PyResult<u64> {
        let handles: Vec<Handle> = items.iter().map(|&v| Handle(v)).collect();
        wrap_result!(self.emu_mut()?.new_java_list(class, &handles).map(|h| h.0))
    }

    fn java_list(&self, handle: u64) -> PyResult<Vec<u64>> {
        wrap_result!(self
            .emu_mut()?
            .java_list(Handle(handle))
            .map(|v| v.iter().map(|h| h.0).collect()))
    }

    fn new_reflected_method(
        &self,
        class: &str,
        name: &str,
        signature: &str,
        is_static: bool,
    ) -> PyResult<u64> {
        Ok(self
            .emu_mut()?
            .new_reflected_method(class, name, signature, is_static)
            .0)
    }

    fn new_reflected_field(
        &self,
        class: &str,
        name: &str,
        signature: &str,
        is_static: bool,
    ) -> PyResult<u64> {
        Ok(self
            .emu_mut()?
            .new_reflected_field(class, name, signature, is_static)
            .0)
    }

    fn reflected_member(&self, handle: u64) -> PyResult<PyReflectedMember> {
        wrap_result!(self
            .emu()?
            .reflected_member(Handle(handle))
            .map(|m| PyReflectedMember {
                class: m.class,
                name: m.name,
                signature: m.signature,
                is_static: m.is_static,
                kind: match m.kind {
                    ReflectedMemberKind::Method => "Method",
                    ReflectedMemberKind::Constructor => "Constructor",
                    ReflectedMemberKind::Field => "Field",
                },
            }))
    }

    fn add_file(&self, path: &str, bytes: Vec<u8>) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.add_file(path, &bytes))
    }

    fn add_host_file(&self, path: &str, host_path: &str) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.add_host_file(path, host_path))
    }

    fn remove_file(&self, path: &str) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.remove_file(path))
    }

    fn file(&self, path: &str) -> PyResult<Vec<u8>> {
        wrap_result!(self.emu()?.file(path))
    }

    fn modules(&self) -> PyResult<Vec<PyModuleInfo>> {
        Ok(self
            .emu()?
            .modules()
            .into_iter()
            .map(|m| PyModuleInfo {
                name: m.name,
                path: m.path.to_string_lossy().into_owned(),
                base: m.base,
                size: m.size,
            })
            .collect())
    }

    fn location(&self, address: u64) -> PyResult<String> {
        Ok(self.emu()?.location(address))
    }

    fn symbolize(&self, address: u64) -> PyResult<String> {
        Ok(self.emu()?.symbolize(address))
    }

    fn resolve(&self, text: &str) -> PyResult<u64> {
        wrap_result!(self.emu()?.resolve(text))
    }

    fn registers(&self) -> PyResult<Vec<(String, u64)>> {
        wrap_result!(self
            .emu()?
            .registers()
            .map(|v| v.into_iter().map(|(n, v)| (n.into(), v)).collect()))
    }

    fn register(&self, name: &str) -> PyResult<u64> {
        wrap_result!(self.emu()?.register(name))
    }

    fn set_register(&self, name: &str, value: u64) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.set_register(name, value))
    }

    fn maps(&self) -> PyResult<Vec<PyMapping>> {
        wrap_result!(self.emu()?.maps().map(|v| v
            .into_iter()
            .map(|m| PyMapping {
                start: m.start,
                end: m.end,
                perms: m.perms,
                name: m.name,
            })
            .collect()))
    }

    fn backtrace(&self) -> PyResult<Vec<PyFrame>> {
        wrap_result!(self.emu()?.backtrace().map(|v| v
            .into_iter()
            .map(|f| PyFrame {
                address: f.address,
                location: f.location,
                symbol: f.symbol,
            })
            .collect()))
    }

    fn add_breakpoint(&self, address: u64) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.add_breakpoint(address))
    }

    fn add_conditional_breakpoint(&self, address: u64, condition: &str) -> PyResult<()> {
        wrap_result!(self
            .emu_mut()?
            .add_conditional_breakpoint(address, condition))
    }

    fn remove_breakpoint(&self, address: u64) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.remove_breakpoint(address))
    }

    fn add_watchpoint(&self, address: u64, size: u64, watch: &str, action: &str) -> PyResult<()> {
        let watch_kind = match watch {
            "read" => Watch::Read,
            "write" => Watch::Write,
            "access" => Watch::Access,
            _ => {
                return Err(PyRuntimeError::new_err(format!(
                    "invalid watch kind: {watch}"
                )))
            }
        };
        let watch_action = match action {
            "stop" => WatchAction::Stop,
            "log" => WatchAction::Log,
            _ => {
                return Err(PyRuntimeError::new_err(format!(
                    "invalid watch action: {action}"
                )))
            }
        };
        wrap_result!(self
            .emu_mut()?
            .add_watchpoint(address, size, watch_kind, watch_action))
    }

    fn remove_watchpoint(&self, address: u64) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.remove_watchpoint(address))
    }

    fn add_tracepoint(&self, address: u64, values: Vec<String>, log: bool) -> PyResult<()> {
        wrap_result!(self
            .emu_mut()?
            .add_tracepoint(address, &Tracepoint { values, log }))
    }

    fn remove_tracepoint(&self, address: u64) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.remove_tracepoint(address))
    }

    fn tracepoint_hits(&self, address: u64) -> PyResult<Vec<(u64, u64)>> {
        wrap_result!(self.emu()?.tracepoint_hits(address))
    }

    fn reset_tracepoint_hits(&self) -> PyResult<()> {
        self.emu_mut()?.reset_tracepoint_hits();
        Ok(())
    }

    fn coverage(&self, py: Python<'_>) -> PyResult<Vec<PyBlockCount>> {
        let _guard = self.enter_detach()?;
        let ptr = self.ptr_usize();
        catch_unwind(AssertUnwindSafe(|| {
            let result = py.detach(move || {
                let emu = unsafe { &*(ptr as *const Emulator) };
                emu.coverage()
                    .into_iter()
                    .map(|b| PyBlockCount {
                        address: b.address,
                        location: b.location,
                        count: b.count,
                    })
                    .collect::<Vec<_>>()
            });
            Ok(result)
        }))
    }

    fn reset_coverage(&self) -> PyResult<()> {
        self.emu_mut()?.reset_coverage();
        Ok(())
    }

    fn search(&self, py: Python<'_>, pattern: Vec<u8>) -> PyResult<Vec<u64>> {
        let _guard = self.enter_detach()?;
        let ptr = self.ptr_usize();
        catch_unwind(AssertUnwindSafe(|| {
            let result: Result<Vec<u64>, String> = py.detach(move || {
                let emu = unsafe { &*(ptr as *const Emulator) };
                emu.search(&pattern).map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn crypto_constants(&self, py: Python<'_>) -> PyResult<Vec<PyCryptoConstant>> {
        let _guard = self.enter_detach()?;
        let ptr = self.ptr_usize();
        catch_unwind(AssertUnwindSafe(|| {
            let result: Result<Vec<PyCryptoConstant>, String> = py.detach(move || {
                let emu = unsafe { &*(ptr as *const Emulator) };
                emu.crypto_constants()
                    .map(|v| {
                        v.into_iter()
                            .map(|c| PyCryptoConstant {
                                name: c.name.to_string(),
                                address: c.address,
                                location: c.location,
                            })
                            .collect()
                    })
                    .map_err(err_to_string)
            });
            result.map_err(PyRuntimeError::new_err)
        }))
    }

    fn checkpoint(&self) -> PyResult<PyCheckpoint> {
        wrap_result!(self.emu_mut()?.checkpoint().map(PyCheckpoint))
    }

    fn rewind(&self, checkpoint: &PyCheckpoint) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.rewind(&checkpoint.0))
    }

    fn snapshot(&self) -> PyResult<PySnapshot> {
        wrap_result!(self.emu()?.snapshot().map(|s| PySnapshot {
            thread: s.thread,
            #[allow(clippy::map_identity)]
            registers: s.registers.into_iter().map(|(n, v)| (n, v)).collect(),
            modules: s
                .modules
                .into_iter()
                .map(|m| PyModuleInfo {
                    name: m.name,
                    path: m.path.to_string_lossy().into_owned(),
                    base: m.base,
                    size: m.size,
                })
                .collect(),
            memory: s
                .memory
                .into_iter()
                .map(|(m, bytes)| (
                    PyMapping {
                        start: m.start,
                        end: m.end,
                        perms: m.perms,
                        name: m.name,
                    },
                    bytes
                ))
                .collect(),
            heap: s
                .heap
                .into_iter()
                .map(PyAllocation::from_allocation)
                .collect(),
        }))
    }

    fn trace(&self) -> PyResult<PyTrace> {
        let guard = self.emu()?;
        let t = guard.trace();
        Ok(PyTrace {
            jni: t.jni,
            syscalls: t.syscalls,
            imports: t.imports,
            heap: t.heap,
            code: t.code.clone(),
            calls: t.calls.clone(),
            coverage: t.coverage.clone(),
        })
    }

    fn set_trace(&self, trace: &PyTrace) -> PyResult<()> {
        wrap_result!(self.emu_mut()?.set_trace(Trace {
            jni: trace.jni,
            syscalls: trace.syscalls,
            imports: trace.imports,
            heap: trace.heap,
            code: trace.code.clone(),
            calls: trace.calls.clone(),
            coverage: trace.coverage.clone(),
        }))
    }

    fn allocation(&self, address: u64) -> PyResult<Option<PyAllocation>> {
        Ok(self
            .emu()?
            .allocation(address)
            .map(PyAllocation::from_allocation))
    }

    fn object(&self, address: u64, depth: usize) -> PyResult<PyHeapObject> {
        wrap_result!(self
            .emu()?
            .object(address, depth)
            .map(PyHeapObject::from_heap_object))
    }

    fn module_bytes(&self, module: usize) -> PyResult<Vec<u8>> {
        wrap_result!(self.emu()?.module_bytes(Module(module)).map(|b| b.to_vec()))
    }

    fn apk_info(&self) -> PyResult<Option<PyApk>> {
        Ok(self.emu()?.apk().map(|a| PyApk {
            path: a.path.to_string_lossy().into_owned(),
            package: a.package.clone(),
            version_name: a.version_name.clone(),
            version_code: a.version_code,
            signatures: a.signatures.clone(),
            signers: a.signers.clone(),
            signing_history: a.signing_history.clone(),
        }))
    }

    fn heap(&self) -> PyResult<Vec<PyAllocation>> {
        Ok(self
            .emu()?
            .heap()
            .into_iter()
            .map(PyAllocation::from_allocation)
            .collect())
    }

    fn reset_heap(&self) -> PyResult<()> {
        self.emu_mut()?.reset_heap();
        Ok(())
    }
}

fn parse_primitive_type(kind: &str) -> PyResult<PrimitiveType> {
    match kind {
        "Boolean" => Ok(PrimitiveType::Boolean),
        "Byte" => Ok(PrimitiveType::Byte),
        "Char" => Ok(PrimitiveType::Char),
        "Short" => Ok(PrimitiveType::Short),
        "Int" => Ok(PrimitiveType::Int),
        "Long" => Ok(PrimitiveType::Long),
        "Float" => Ok(PrimitiveType::Float),
        "Double" => Ok(PrimitiveType::Double),
        _ => Err(PyRuntimeError::new_err(format!(
            "invalid primitive type: {kind}; expected Boolean, Byte, Char, Short, Int, Long, Float, Double"
        ))),
    }
}

struct PythonJniHandler {
    handler: Py<PyAny>,
    py_emu: Py<PyEmulator>,
}

impl PythonJniHandler {
    /// CHANGED: 统一将 Python 异常记录到所属 `PyEmulator` 的 `last_errors`。
    fn record_error(&self, py: Python<'_>, err: PyErr) {
        if let Ok(emu) = self.py_emu.bind(py).try_borrow() {
            let mut errs = emu.last_errors.lock().unwrap_or_else(|p| p.into_inner());
            errs.push(err.to_string());
        }
    }

    fn make_call_dict<'py>(
        &self,
        py: Python<'py>,
        call: &JniCall<'_>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        dict.set_item("op", call.op.name())?;
        dict.set_item("is_static", call.is_static)?;
        dict.set_item(
            "type_code",
            std::str::from_utf8(&[call.type_code]).unwrap_or("?"),
        )?;
        dict.set_item("class", call.class)?;
        dict.set_item("name", call.name)?;
        dict.set_item("signature", call.signature)?;
        dict.set_item("receiver", call.receiver.0)?;
        let args: Vec<Bound<'py, PyAny>> = call
            .args
            .iter()
            .map(|v| jni_value_to_py(py, *v))
            .collect::<PyResult<_>>()?;
        dict.set_item("args", args)?;
        Ok(dict)
    }
}

/// CHANGED: `bool::into_pyobject` returns `Borrowed<PyBool>`, which needs `.to_owned()`
/// to convert to `Bound<PyBool>` before `.into_any()` can consume it.
fn jni_value_to_py(py: Python<'_>, value: JniValue) -> PyResult<Bound<'_, PyAny>> {
    match value {
        JniValue::Object(h) => Ok(h.0.into_pyobject(py)?.into_any()),
        JniValue::Boolean(v) => Ok(v.into_pyobject(py)?.to_owned().into_any()),
        JniValue::Byte(v) => Ok(v.into_pyobject(py)?.into_any()),
        JniValue::Char(v) => Ok(v.into_pyobject(py)?.into_any()),
        JniValue::Short(v) => Ok(v.into_pyobject(py)?.into_any()),
        JniValue::Int(v) => Ok(v.into_pyobject(py)?.into_any()),
        JniValue::Long(v) => Ok(v.into_pyobject(py)?.into_any()),
        JniValue::Float(v) => Ok(v.into_pyobject(py)?.into_any()),
        JniValue::Double(v) => Ok(v.into_pyobject(py)?.into_any()),
        JniValue::Void => Ok(py.None().into_bound(py).into_any()),
    }
}

impl JniHandler for PythonJniHandler {
    fn import(&mut self, _emu: &mut Emulator, call: &ImportCall<'_>) -> Option<u64> {
        Python::attach(|py| {
            let handler = self.handler.bind(py);
            let py_emu = self.py_emu.bind(py);
            let args_dict = PyDict::new(py);
            if args_dict.set_item("name", call.name).is_err() {
                return None;
            }
            if args_dict.set_item("args", call.args.to_vec()).is_err() {
                return None;
            }
            if let Some(original) = call.original {
                if args_dict.set_item("original", original).is_err() {
                    return None;
                }
            }
            if args_dict
                .set_item("return_address", call.return_address)
                .is_err()
            {
                return None;
            }
            let result = handler.call_method1("on_import", (py_emu, args_dict));
            match result {
                Ok(r) => r.extract::<u64>().ok(),
                Err(e) => {
                    self.record_error(py, e);
                    None
                }
            }
        })
    }

    fn stopped(&mut self, _emu: &mut Emulator, stop: &Stop) -> Resume {
        Python::attach(|py| {
            let handler = self.handler.bind(py);
            let py_emu = self.py_emu.bind(py);
            let stop_dict = PyDict::new(py);
            if stop_dict
                .set_item(
                    "reason",
                    match stop.reason {
                        StopReason::Breakpoint => "Breakpoint",
                        StopReason::Watchpoint => "Watchpoint",
                        StopReason::Step => "Step",
                        StopReason::Finish => "Finish",
                    },
                )
                .is_err()
            {
                return Resume::Continue;
            }
            if stop_dict.set_item("address", stop.address).is_err() {
                return Resume::Continue;
            }
            if stop_dict.set_item("thread", stop.thread).is_err() {
                return Resume::Continue;
            }
            if let Some(hit) = &stop.watch {
                let watch_dict = PyDict::new(py);
                if watch_dict.set_item("pc", hit.pc).is_err() {
                    return Resume::Continue;
                }
                if watch_dict.set_item("address", hit.address).is_err() {
                    return Resume::Continue;
                }
                if watch_dict.set_item("size", hit.size).is_err() {
                    return Resume::Continue;
                }
                if watch_dict.set_item("write", hit.write).is_err() {
                    return Resume::Continue;
                }
                if watch_dict.set_item("value", hit.value).is_err() {
                    return Resume::Continue;
                }
                if stop_dict.set_item("watch", watch_dict).is_err() {
                    return Resume::Continue;
                }
            }
            let result = handler.call_method1("stopped", (py_emu, stop_dict));
            match result {
                Ok(r) => {
                    if let Ok(s) = r.extract::<String>() {
                        match s.as_str() {
                            "Continue" => Resume::Continue,
                            "Step" => Resume::Step,
                            "Finish" => Resume::Finish,
                            "Abort" => Resume::Abort,
                            _ => Resume::Continue,
                        }
                    } else if let Ok(value) = r.extract::<u64>() {
                        Resume::Return(value)
                    } else {
                        Resume::Continue
                    }
                }
                Err(e) => {
                    self.record_error(py, e);
                    Resume::Continue
                }
            }
        })
    }

    fn is_assignable(&mut self, _emu: &mut Emulator, from: &str, to: &str) -> Option<bool> {
        Python::attach(|py| {
            let handler = self.handler.bind(py);
            let py_emu = self.py_emu.bind(py);
            match handler.call_method1("is_assignable", (py_emu, from, to)) {
                Ok(r) => r.extract::<bool>().ok(),
                Err(e) => {
                    self.record_error(py, e);
                    None
                }
            }
        })
    }

    fn member(&mut self, _emu: &mut Emulator, call: &JniCall<'_>) -> Option<JniValue> {
        let type_code = call.type_code;
        Python::attach(|py| {
            let handler = self.handler.bind(py);
            let py_emu = self.py_emu.bind(py);
            let call_dict = match self.make_call_dict(py, call) {
                Ok(d) => d,
                Err(e) => {
                    self.record_error(py, e);
                    return None;
                }
            };
            match handler.call_method1("member", (py_emu, call_dict)) {
                Ok(r) => py_to_jni_value(type_code, r.as_ref()).ok(),
                Err(e) => {
                    self.record_error(py, e);
                    None
                }
            }
        })
    }
}

#[pyclass(name = "JniValue", from_py_object)]
#[derive(Clone)]
struct PyJniValue(JniValue);

#[pymethods]
impl PyJniValue {
    #[staticmethod]
    fn object(handle: u64) -> Self {
        PyJniValue(JniValue::Object(Handle(handle)))
    }
    #[staticmethod]
    fn boolean(v: bool) -> Self {
        PyJniValue(JniValue::Boolean(v))
    }
    #[staticmethod]
    fn byte(v: i8) -> Self {
        PyJniValue(JniValue::Byte(v))
    }
    #[staticmethod]
    fn char(v: u16) -> Self {
        PyJniValue(JniValue::Char(v))
    }
    #[staticmethod]
    fn short(v: i16) -> Self {
        PyJniValue(JniValue::Short(v))
    }
    #[staticmethod]
    fn int(v: i32) -> Self {
        PyJniValue(JniValue::Int(v))
    }
    #[staticmethod]
    fn long(v: i64) -> Self {
        PyJniValue(JniValue::Long(v))
    }
    #[staticmethod]
    fn float_(v: f32) -> Self {
        PyJniValue(JniValue::Float(v))
    }
    #[staticmethod]
    fn double(v: f64) -> Self {
        PyJniValue(JniValue::Double(v))
    }
    #[staticmethod]
    fn void() -> Self {
        PyJniValue(JniValue::Void)
    }

    fn kind(&self) -> &str {
        match self.0 {
            JniValue::Object(_) => "Object",
            JniValue::Boolean(_) => "Boolean",
            JniValue::Byte(_) => "Byte",
            JniValue::Char(_) => "Char",
            JniValue::Short(_) => "Short",
            JniValue::Int(_) => "Int",
            JniValue::Long(_) => "Long",
            JniValue::Float(_) => "Float",
            JniValue::Double(_) => "Double",
            JniValue::Void => "Void",
        }
    }

    fn __repr__(&self) -> String {
        match &self.0 {
            JniValue::Object(h) => format!("JniValue(Object({}))", h.0),
            JniValue::Boolean(v) => format!("JniValue(Boolean({}))", v),
            JniValue::Byte(v) => format!("JniValue(Byte({}))", v),
            JniValue::Char(v) => format!("JniValue(Char({}))", v),
            JniValue::Short(v) => format!("JniValue(Short({}))", v),
            JniValue::Int(v) => format!("JniValue(Int({}))", v),
            JniValue::Long(v) => format!("JniValue(Long({}))", v),
            JniValue::Float(v) => format!("JniValue(Float({}))", v),
            JniValue::Double(v) => format!("JniValue(Double({}))", v),
            JniValue::Void => "JniValue(Void)".into(),
        }
    }

    /// CHANGED: `bool::into_pyobject` returns `Borrowed<PyBool>`, which needs `.to_owned()`
    /// to convert to `Bound<PyBool>` before `.into_any()` can consume it.
    fn value<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match &self.0 {
            JniValue::Object(h) => Ok(h.0.into_pyobject(py)?.into_any()),
            JniValue::Boolean(v) => Ok(v.into_pyobject(py)?.to_owned().into_any()),
            JniValue::Byte(v) => Ok(v.into_pyobject(py)?.into_any()),
            JniValue::Char(v) => Ok(v.into_pyobject(py)?.into_any()),
            JniValue::Short(v) => Ok(v.into_pyobject(py)?.into_any()),
            JniValue::Int(v) => Ok(v.into_pyobject(py)?.into_any()),
            JniValue::Long(v) => Ok(v.into_pyobject(py)?.into_any()),
            JniValue::Float(v) => Ok(v.into_pyobject(py)?.into_any()),
            JniValue::Double(v) => Ok(v.into_pyobject(py)?.into_any()),
            JniValue::Void => Ok(py.None().into_bound(py).into_any()),
        }
    }
}

#[pyclass(name = "Trace", skip_from_py_object)]
#[derive(Clone)]
struct PyTrace {
    #[pyo3(get)]
    jni: bool,
    #[pyo3(get)]
    syscalls: bool,
    #[pyo3(get)]
    imports: bool,
    #[pyo3(get)]
    heap: bool,
    #[pyo3(get)]
    code: Vec<String>,
    #[pyo3(get)]
    calls: Vec<String>,
    #[pyo3(get)]
    coverage: Vec<String>,
}

#[pymethods]
impl PyTrace {
    #[new]
    #[pyo3(signature = (jni=false, syscalls=false, imports=false, heap=false, code=vec![], calls=vec![], coverage=vec![]))]
    fn new(
        jni: bool,
        syscalls: bool,
        imports: bool,
        heap: bool,
        code: Vec<String>,
        calls: Vec<String>,
        coverage: Vec<String>,
    ) -> Self {
        PyTrace {
            jni,
            syscalls,
            imports,
            heap,
            code,
            calls,
            coverage,
        }
    }
}

#[pyclass(name = "ModuleInfo", skip_from_py_object)]
#[derive(Clone)]
struct PyModuleInfo {
    name: String,
    path: String,
    base: u64,
    size: u64,
}

#[pymethods]
impl PyModuleInfo {
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }
    #[getter]
    fn path(&self) -> &str {
        &self.path
    }
    #[getter]
    fn base(&self) -> u64 {
        self.base
    }
    #[getter]
    fn size(&self) -> u64 {
        self.size
    }

    fn __repr__(&self) -> String {
        format!(
            "ModuleInfo(name='{}', base=0x{:x}, size=0x{:x})",
            self.name, self.base, self.size
        )
    }
}

#[pyclass(name = "Mapping", skip_from_py_object)]
#[derive(Clone)]
struct PyMapping {
    start: u64,
    end: u64,
    perms: String,
    name: String,
}

#[pymethods]
impl PyMapping {
    #[getter]
    fn start(&self) -> u64 {
        self.start
    }
    #[getter]
    fn end(&self) -> u64 {
        self.end
    }
    #[getter]
    fn perms(&self) -> &str {
        &self.perms
    }
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    fn __repr__(&self) -> String {
        format!(
            "Mapping(0x{:x}-0x{:x} {} '{}')",
            self.start, self.end, self.perms, self.name
        )
    }
}

#[pyclass(name = "Frame", skip_from_py_object)]
#[derive(Clone)]
struct PyFrame {
    address: u64,
    location: String,
    symbol: Option<String>,
}

#[pymethods]
impl PyFrame {
    #[getter]
    fn address(&self) -> u64 {
        self.address
    }
    #[getter]
    fn location(&self) -> &str {
        &self.location
    }
    #[getter]
    fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }

    fn __repr__(&self) -> String {
        match &self.symbol {
            Some(s) => format!("Frame(0x{:x} {} [{}])", self.address, self.location, s),
            None => format!("Frame(0x{:x} {})", self.address, self.location),
        }
    }
}

#[pyclass(name = "ReflectedMember", skip_from_py_object)]
#[derive(Clone)]
struct PyReflectedMember {
    class: String,
    name: String,
    signature: String,
    is_static: bool,
    kind: &'static str,
}

#[pymethods]
impl PyReflectedMember {
    #[getter]
    fn class(&self) -> &str {
        &self.class
    }
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }
    #[getter]
    fn signature(&self) -> &str {
        &self.signature
    }
    #[getter]
    fn is_static(&self) -> bool {
        self.is_static
    }
    #[getter]
    fn kind(&self) -> &str {
        self.kind
    }
}

#[pyclass(name = "BlockCount", skip_from_py_object)]
#[derive(Clone)]
struct PyBlockCount {
    address: u64,
    location: String,
    count: u64,
}

#[pymethods]
impl PyBlockCount {
    #[getter]
    fn address(&self) -> u64 {
        self.address
    }
    #[getter]
    fn location(&self) -> &str {
        &self.location
    }
    #[getter]
    fn count(&self) -> u64 {
        self.count
    }
}

#[pyclass(name = "CryptoConstant", skip_from_py_object)]
#[derive(Clone)]
struct PyCryptoConstant {
    name: String,
    address: u64,
    location: String,
}

#[pymethods]
impl PyCryptoConstant {
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }
    #[getter]
    fn address(&self) -> u64 {
        self.address
    }
    #[getter]
    fn location(&self) -> &str {
        &self.location
    }
}

/// A checkpoint bound to the thread that created it.
///
/// `Checkpoint` may contain non-`Send` internal state. PyO3 raises
/// `RuntimeError` (via panic + catch) if this object is used from a
/// different thread. Create and use checkpoints on the same thread.
#[pyclass(name = "Checkpoint", unsendable)]
struct PyCheckpoint(Checkpoint);

#[pyclass(name = "Snapshot", skip_from_py_object)]
#[derive(Clone)]
struct PySnapshot {
    thread: u32,
    registers: Vec<(String, u64)>,
    modules: Vec<PyModuleInfo>,
    memory: Vec<(PyMapping, Vec<u8>)>,
    heap: Vec<PyAllocation>,
}

#[pymethods]
impl PySnapshot {
    #[getter]
    fn thread(&self) -> u32 {
        self.thread
    }
    #[getter]
    fn registers(&self) -> Vec<(String, u64)> {
        self.registers.clone()
    }
    #[getter]
    fn modules(&self) -> Vec<PyModuleInfo> {
        self.modules.clone()
    }
    #[getter]
    fn memory(&self) -> Vec<(PyMapping, Vec<u8>)> {
        self.memory.clone()
    }
    #[getter]
    fn heap(&self) -> Vec<PyAllocation> {
        self.heap.clone()
    }

    fn save(&self, path: &str) -> PyResult<()> {
        wrap_result!(CoreSnapshot {
            thread: self.thread,
            registers: self.registers.clone(),
            modules: self
                .modules
                .iter()
                .map(|m| ModuleInfo {
                    name: m.name.clone(),
                    path: std::path::PathBuf::from(&m.path),
                    base: m.base,
                    size: m.size,
                })
                .collect(),
            memory: self
                .memory
                .iter()
                .map(|(m, bytes)| {
                    let mapping = Mapping {
                        start: m.start,
                        end: m.end,
                        perms: m.perms.clone(),
                        name: m.name.clone(),
                    };
                    (mapping, bytes.clone())
                })
                .collect(),
            heap: self.heap.iter().map(|a| a.to_allocation()).collect(),
        }
        .save(path))
    }

    #[staticmethod]
    fn load(path: &str) -> PyResult<PySnapshot> {
        wrap_result!(CoreSnapshot::load(path).map(|s| PySnapshot {
            thread: s.thread,
            #[allow(clippy::map_identity)]
            registers: s.registers.into_iter().map(|(n, v)| (n, v)).collect(),
            modules: s
                .modules
                .into_iter()
                .map(|m| PyModuleInfo {
                    name: m.name,
                    path: m.path.to_string_lossy().into_owned(),
                    base: m.base,
                    size: m.size,
                })
                .collect(),
            memory: s
                .memory
                .into_iter()
                .map(|(m, bytes)| (
                    PyMapping {
                        start: m.start,
                        end: m.end,
                        perms: m.perms,
                        name: m.name,
                    },
                    bytes
                ))
                .collect(),
            heap: s
                .heap
                .into_iter()
                .map(PyAllocation::from_allocation)
                .collect(),
        }))
    }
}

#[pyclass(name = "Allocation", skip_from_py_object)]
#[derive(Clone)]
struct PyAllocation {
    address: u64,
    size: u64,
    live: bool,
    function: String,
    thread: u32,
    backtrace: Vec<PyFrame>,
    freed: Option<Vec<PyFrame>>,
}

impl PyAllocation {
    fn from_allocation(a: Allocation) -> Self {
        PyAllocation {
            address: a.address,
            size: a.size,
            live: a.live,
            function: a.function,
            thread: a.thread,
            backtrace: a
                .backtrace
                .into_iter()
                .map(|f| PyFrame {
                    address: f.address,
                    location: f.location,
                    symbol: f.symbol,
                })
                .collect(),
            freed: a.freed.map(|frames| {
                frames
                    .into_iter()
                    .map(|f| PyFrame {
                        address: f.address,
                        location: f.location,
                        symbol: f.symbol,
                    })
                    .collect()
            }),
        }
    }

    fn to_allocation(&self) -> Allocation {
        Allocation {
            address: self.address,
            size: self.size,
            live: self.live,
            function: self.function.clone(),
            thread: self.thread,
            backtrace: self
                .backtrace
                .iter()
                .map(|f| unidroid_core::Frame {
                    address: f.address,
                    location: f.location.clone(),
                    symbol: f.symbol.clone(),
                })
                .collect(),
            freed: self.freed.as_ref().map(|frames| {
                frames
                    .iter()
                    .map(|f| unidroid_core::Frame {
                        address: f.address,
                        location: f.location.clone(),
                        symbol: f.symbol.clone(),
                    })
                    .collect()
            }),
        }
    }
}

#[pymethods]
impl PyAllocation {
    #[getter]
    fn address(&self) -> u64 {
        self.address
    }
    #[getter]
    fn size(&self) -> u64 {
        self.size
    }
    #[getter]
    fn live(&self) -> bool {
        self.live
    }
    #[getter]
    fn function(&self) -> &str {
        &self.function
    }
    #[getter]
    fn thread(&self) -> u32 {
        self.thread
    }
    #[getter]
    fn backtrace(&self) -> Vec<PyFrame> {
        self.backtrace.clone()
    }
    #[getter]
    fn freed(&self) -> Option<Vec<PyFrame>> {
        self.freed.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "Allocation(0x{:x} size=0x{:x} {} thread={} live={})",
            self.address, self.size, self.function, self.thread, self.live
        )
    }
}

#[pyclass(name = "HeapObject", skip_from_py_object)]
#[derive(Clone)]
struct PyHeapObject {
    address: u64,
    size: u64,
    allocation: PyAllocation,
    fields: Vec<PyField>,
    referrers: Vec<PyReferrer>,
}

impl PyHeapObject {
    fn from_heap_object(obj: HeapObject) -> Self {
        PyHeapObject {
            address: obj.address,
            size: obj.size,
            allocation: PyAllocation::from_allocation(obj.allocation),
            fields: obj.fields.into_iter().map(PyField::from_field).collect(),
            referrers: obj
                .referrers
                .into_iter()
                .map(PyReferrer::from_referrer)
                .collect(),
        }
    }
}

#[pymethods]
impl PyHeapObject {
    #[getter]
    fn address(&self) -> u64 {
        self.address
    }
    #[getter]
    fn size(&self) -> u64 {
        self.size
    }
    #[getter]
    fn allocation(&self) -> PyAllocation {
        self.allocation.clone()
    }
    #[getter]
    fn fields(&self) -> Vec<PyField> {
        self.fields.clone()
    }
    #[getter]
    fn referrers(&self) -> Vec<PyReferrer> {
        self.referrers.clone()
    }
}

#[pyclass(name = "Field", skip_from_py_object)]
#[derive(Clone)]
struct PyField {
    offset: u64,
    value: u64,
    target: Option<PyTarget>,
    string: Option<String>,
}

impl PyField {
    fn from_field(f: Field) -> Self {
        PyField {
            offset: f.offset,
            value: f.value,
            target: f.target.map(PyTarget::from_target),
            string: f.string,
        }
    }
}

#[pymethods]
impl PyField {
    #[getter]
    fn offset(&self) -> u64 {
        self.offset
    }
    #[getter]
    fn value(&self) -> u64 {
        self.value
    }
    #[getter]
    fn target(&self) -> Option<PyTarget> {
        self.target.clone()
    }
    #[getter]
    fn string(&self) -> Option<&str> {
        self.string.as_deref()
    }
}

#[pyclass(name = "Target", skip_from_py_object)]
#[derive(Clone)]
struct PyTarget {
    kind: String,
    address: Option<u64>,
    offset: Option<u64>,
    object: Option<PyHeapObject>,
    location: Option<String>,
}

impl PyTarget {
    fn from_target(t: Target) -> Self {
        match t {
            Target::Block {
                address,
                offset,
                object,
            } => PyTarget {
                kind: "block".into(),
                address: Some(address),
                offset: Some(offset),
                object: object.map(|o| PyHeapObject::from_heap_object(*o)),
                location: None,
            },
            Target::Location { location } => PyTarget {
                kind: "location".into(),
                address: None,
                offset: None,
                object: None,
                location: Some(location),
            },
        }
    }
}

#[pymethods]
impl PyTarget {
    #[getter]
    fn kind(&self) -> &str {
        &self.kind
    }
    #[getter]
    fn address(&self) -> Option<u64> {
        self.address
    }
    #[getter]
    fn offset(&self) -> Option<u64> {
        self.offset
    }
    #[getter]
    fn object(&self) -> Option<PyHeapObject> {
        self.object.clone()
    }
    #[getter]
    fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }
}

#[pyclass(name = "Referrer", skip_from_py_object)]
#[derive(Clone)]
struct PyReferrer {
    address: u64,
    location: String,
    block: Option<u64>,
    object: Option<PyHeapObject>,
}

impl PyReferrer {
    fn from_referrer(r: Referrer) -> Self {
        PyReferrer {
            address: r.address,
            location: r.location,
            block: r.block,
            object: r.object.map(|o| PyHeapObject::from_heap_object(*o)),
        }
    }
}

#[pymethods]
impl PyReferrer {
    #[getter]
    fn address(&self) -> u64 {
        self.address
    }
    #[getter]
    fn location(&self) -> &str {
        &self.location
    }
    #[getter]
    fn block(&self) -> Option<u64> {
        self.block
    }
    #[getter]
    fn object(&self) -> Option<PyHeapObject> {
        self.object.clone()
    }
}

#[pyclass(name = "ApkInfo", skip_from_py_object)]
#[derive(Clone)]
struct PyApk {
    path: String,
    package: String,
    version_name: Option<String>,
    version_code: i64,
    signatures: Vec<Vec<u8>>,
    signers: Vec<Vec<u8>>,
    signing_history: Vec<Vec<u8>>,
}

#[pymethods]
impl PyApk {
    #[getter]
    fn path(&self) -> &str {
        &self.path
    }
    #[getter]
    fn package(&self) -> &str {
        &self.package
    }
    #[getter]
    fn version_name(&self) -> Option<&str> {
        self.version_name.as_deref()
    }
    #[getter]
    fn version_code(&self) -> i64 {
        self.version_code
    }
    #[getter]
    fn signatures(&self) -> Vec<Vec<u8>> {
        self.signatures.clone()
    }
    #[getter]
    fn signers(&self) -> Vec<Vec<u8>> {
        self.signers.clone()
    }
    #[getter]
    fn signing_history(&self) -> Vec<Vec<u8>> {
        self.signing_history.clone()
    }
}

#[pymodule]
fn unidroid(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyEmulator>()?;
    m.add_class::<PyJniValue>()?;
    m.add_class::<PyTrace>()?;
    m.add_class::<PyModuleInfo>()?;
    m.add_class::<PyMapping>()?;
    m.add_class::<PyFrame>()?;
    m.add_class::<PyReflectedMember>()?;
    m.add_class::<PyBlockCount>()?;
    m.add_class::<PyCryptoConstant>()?;
    m.add_class::<PyCheckpoint>()?;
    m.add_class::<PySnapshot>()?;
    m.add_class::<PyAllocation>()?;
    m.add_class::<PyHeapObject>()?;
    m.add_class::<PyField>()?;
    m.add_class::<PyTarget>()?;
    m.add_class::<PyReferrer>()?;
    m.add_class::<PyApk>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_primitive_type_valid() {
        assert!(parse_primitive_type("Boolean").is_ok());
        assert!(parse_primitive_type("Byte").is_ok());
        assert!(parse_primitive_type("Char").is_ok());
        assert!(parse_primitive_type("Short").is_ok());
        assert!(parse_primitive_type("Int").is_ok());
        assert!(parse_primitive_type("Long").is_ok());
        assert!(parse_primitive_type("Float").is_ok());
        assert!(parse_primitive_type("Double").is_ok());
    }

    #[test]
    fn parse_primitive_type_values() {
        assert!(matches!(
            parse_primitive_type("Boolean").unwrap(),
            PrimitiveType::Boolean
        ));
        assert!(matches!(
            parse_primitive_type("Byte").unwrap(),
            PrimitiveType::Byte
        ));
        assert!(matches!(
            parse_primitive_type("Char").unwrap(),
            PrimitiveType::Char
        ));
        assert!(matches!(
            parse_primitive_type("Short").unwrap(),
            PrimitiveType::Short
        ));
        assert!(matches!(
            parse_primitive_type("Int").unwrap(),
            PrimitiveType::Int
        ));
        assert!(matches!(
            parse_primitive_type("Long").unwrap(),
            PrimitiveType::Long
        ));
        assert!(matches!(
            parse_primitive_type("Float").unwrap(),
            PrimitiveType::Float
        ));
        assert!(matches!(
            parse_primitive_type("Double").unwrap(),
            PrimitiveType::Double
        ));
    }

    #[test]
    fn parse_primitive_type_invalid() {
        let err = parse_primitive_type("String").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid primitive type: String"));
        assert!(msg.contains("expected Boolean, Byte, Char, Short, Int, Long, Float, Double"));
    }

    #[test]
    fn allocation_roundtrip() {
        let alloc = Allocation {
            address: 0x1000,
            size: 0x200,
            live: true,
            function: "malloc".into(),
            thread: 1,
            backtrace: vec![unidroid_core::Frame {
                address: 0x5000,
                location: "libfoo.so!bar".into(),
                symbol: Some("bar".into()),
            }],
            freed: Some(vec![unidroid_core::Frame {
                address: 0x6000,
                location: "libfoo.so!free".into(),
                symbol: None,
            }]),
        };
        let py_alloc = PyAllocation::from_allocation(alloc.clone());
        assert_eq!(py_alloc.address, 0x1000);
        assert_eq!(py_alloc.size, 0x200);
        assert!(py_alloc.live);
        assert_eq!(py_alloc.function, "malloc");
        assert_eq!(py_alloc.thread, 1);
        assert_eq!(py_alloc.backtrace.len(), 1);
        assert_eq!(py_alloc.backtrace[0].address, 0x5000);
        assert!(py_alloc.freed.is_some());
        let roundtrip = py_alloc.to_allocation();
        assert_eq!(roundtrip.address, alloc.address);
        assert_eq!(roundtrip.size, alloc.size);
        assert_eq!(roundtrip.live, alloc.live);
        assert_eq!(roundtrip.function, alloc.function);
        assert_eq!(roundtrip.thread, alloc.thread);
    }

    #[test]
    fn target_from_block() {
        let target = Target::Block {
            address: 0x1000,
            offset: 8,
            object: None,
        };
        let py_target = PyTarget::from_target(target);
        assert_eq!(py_target.kind, "block");
        assert_eq!(py_target.address, Some(0x1000));
        assert_eq!(py_target.offset, Some(8));
        assert!(py_target.object.is_none());
        assert!(py_target.location.is_none());
    }

    #[test]
    fn target_from_location() {
        let target = Target::Location {
            location: "libfoo.so!bar".into(),
        };
        let py_target = PyTarget::from_target(target);
        assert_eq!(py_target.kind, "location");
        assert!(py_target.address.is_none());
        assert!(py_target.offset.is_none());
        assert!(py_target.object.is_none());
        assert_eq!(py_target.location, Some("libfoo.so!bar".into()));
    }

    #[test]
    fn field_from_field() {
        let field = Field {
            offset: 16,
            value: 0xDEAD,
            target: None,
            string: Some("hello".into()),
        };
        let py_field = PyField::from_field(field);
        assert_eq!(py_field.offset, 16);
        assert_eq!(py_field.value, 0xDEAD);
        assert!(py_field.target.is_none());
        assert_eq!(py_field.string, Some("hello".into()));
    }

    #[test]
    fn referrer_from_referrer() {
        let referrer = Referrer {
            address: 0x2000,
            location: "libbar.so!baz".into(),
            block: Some(0x3000),
            object: None,
        };
        let py_ref = PyReferrer::from_referrer(referrer);
        assert_eq!(py_ref.address, 0x2000);
        assert_eq!(py_ref.location, "libbar.so!baz");
        assert_eq!(py_ref.block, Some(0x3000));
        assert!(py_ref.object.is_none());
    }

    #[test]
    fn heap_object_from_heap_object() {
        let alloc = Allocation {
            address: 0x1000,
            size: 0x100,
            live: true,
            function: "new".into(),
            thread: 0,
            backtrace: vec![],
            freed: None,
        };
        let heap_obj = HeapObject {
            address: 0x1000,
            size: 0x100,
            allocation: alloc,
            fields: vec![Field {
                offset: 0,
                value: 42,
                target: None,
                string: None,
            }],
            referrers: vec![],
        };
        let py_obj = PyHeapObject::from_heap_object(heap_obj);
        assert_eq!(py_obj.address, 0x1000);
        assert_eq!(py_obj.size, 0x100);
        assert_eq!(py_obj.fields.len(), 1);
        assert_eq!(py_obj.fields[0].offset, 0);
        assert_eq!(py_obj.fields[0].value, 42);
        assert!(py_obj.referrers.is_empty());
    }

    #[test]
    fn jni_value_kind() {
        assert_eq!(PyJniValue(JniValue::Object(Handle(1))).kind(), "Object");
        assert_eq!(PyJniValue(JniValue::Boolean(true)).kind(), "Boolean");
        assert_eq!(PyJniValue(JniValue::Byte(1)).kind(), "Byte");
        assert_eq!(PyJniValue(JniValue::Char(1)).kind(), "Char");
        assert_eq!(PyJniValue(JniValue::Short(1)).kind(), "Short");
        assert_eq!(PyJniValue(JniValue::Int(1)).kind(), "Int");
        assert_eq!(PyJniValue(JniValue::Long(1)).kind(), "Long");
        assert_eq!(PyJniValue(JniValue::Float(1.0)).kind(), "Float");
        assert_eq!(PyJniValue(JniValue::Double(1.0)).kind(), "Double");
        assert_eq!(PyJniValue(JniValue::Void).kind(), "Void");
    }

    #[test]
    fn jni_value_repr() {
        assert_eq!(
            PyJniValue(JniValue::Object(Handle(42))).__repr__(),
            "JniValue(Object(42))"
        );
        assert_eq!(
            PyJniValue(JniValue::Boolean(true)).__repr__(),
            "JniValue(Boolean(true))"
        );
        assert_eq!(
            PyJniValue(JniValue::Long(-1)).__repr__(),
            "JniValue(Long(-1))"
        );
        assert_eq!(PyJniValue(JniValue::Void).__repr__(), "JniValue(Void)");
    }

    #[test]
    fn emu_guard_drop_releases_busy() {
        let emu = Emulator::builder()
            .rootfs("../rootfs")
            .threads(false)
            .build()
            .expect("failed to build emulator");
        let py_emu = PyEmulator {
            emu: UnsafeCell::new(emu),
            process_name: "test".into(),
            busy: AtomicBool::new(false),
            last_errors: Mutex::new(Vec::new()),
        };
        assert!(!py_emu.busy.load(Ordering::Acquire));
        {
            let guard = py_emu.lock_emu().expect("lock should succeed");
            assert!(py_emu.busy.load(Ordering::Acquire));
            let _ = &*guard;
        }
        assert!(!py_emu.busy.load(Ordering::Acquire));
    }

    #[test]
    fn emu_guard_double_lock_fails() {
        let emu = Emulator::builder()
            .rootfs("../rootfs")
            .threads(false)
            .build()
            .expect("failed to build emulator");
        let py_emu = PyEmulator {
            emu: UnsafeCell::new(emu),
            process_name: "test".into(),
            busy: AtomicBool::new(false),
            last_errors: Mutex::new(Vec::new()),
        };
        let _guard1 = py_emu.lock_emu().expect("first lock should succeed");
        match py_emu.lock_emu() {
            Err(e) => assert!(e.to_string().contains("Emulator is busy")),
            Ok(_) => panic!("second lock should fail"),
        };
    }

    #[test]
    fn emu_guard_panic_safe() {
        let emu = Emulator::builder()
            .rootfs("../rootfs")
            .threads(false)
            .build()
            .expect("failed to build emulator");
        let py_emu = PyEmulator {
            emu: UnsafeCell::new(emu),
            process_name: "test".into(),
            busy: AtomicBool::new(false),
            last_errors: Mutex::new(Vec::new()),
        };
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = py_emu.lock_emu().expect("lock should succeed");
            assert!(py_emu.busy.load(Ordering::Acquire));
            panic!("intentional");
        }));
        assert!(result.is_err());
        assert!(!py_emu.busy.load(Ordering::Acquire));
    }

    #[test]
    fn take_jni_errors_drains() {
        let emu = Emulator::builder()
            .rootfs("../rootfs")
            .threads(false)
            .build()
            .expect("failed to build emulator");
        let py_emu = PyEmulator {
            emu: UnsafeCell::new(emu),
            process_name: "test".into(),
            busy: AtomicBool::new(false),
            last_errors: Mutex::new(vec!["err1".into(), "err2".into()]),
        };
        let errors = py_emu.take_jni_errors();
        assert_eq!(errors, vec!["err1", "err2"]);
        let errors2 = py_emu.take_jni_errors();
        assert!(errors2.is_empty());
    }

    #[test]
    fn reflected_member_kind_mapping() {
        assert_eq!(
            match ReflectedMemberKind::Method {
                ReflectedMemberKind::Method => "Method",
                ReflectedMemberKind::Constructor => "Constructor",
                ReflectedMemberKind::Field => "Field",
            },
            "Method"
        );
        assert_eq!(
            match ReflectedMemberKind::Constructor {
                ReflectedMemberKind::Method => "Method",
                ReflectedMemberKind::Constructor => "Constructor",
                ReflectedMemberKind::Field => "Field",
            },
            "Constructor"
        );
        assert_eq!(
            match ReflectedMemberKind::Field {
                ReflectedMemberKind::Method => "Method",
                ReflectedMemberKind::Constructor => "Constructor",
                ReflectedMemberKind::Field => "Field",
            },
            "Field"
        );
    }

    #[test]
    fn module_info_repr() {
        let info = PyModuleInfo {
            name: "libfoo.so".into(),
            path: "/system/lib/libfoo.so".into(),
            base: 0x1000,
            size: 0x2000,
        };
        assert_eq!(
            info.__repr__(),
            "ModuleInfo(name='libfoo.so', base=0x1000, size=0x2000)"
        );
    }

    #[test]
    fn mapping_repr() {
        let m = PyMapping {
            start: 0x1000,
            end: 0x3000,
            perms: "r-x".into(),
            name: "libfoo.so".into(),
        };
        assert_eq!(m.__repr__(), "Mapping(0x1000-0x3000 r-x 'libfoo.so')");
    }

    #[test]
    fn frame_repr_with_symbol() {
        let f = PyFrame {
            address: 0x5000,
            location: "libfoo.so!bar+0x10".into(),
            symbol: Some("bar".into()),
        };
        assert_eq!(f.__repr__(), "Frame(0x5000 libfoo.so!bar+0x10 [bar])");
    }

    #[test]
    fn frame_repr_without_symbol() {
        let f = PyFrame {
            address: 0x5000,
            location: "libfoo.so".into(),
            symbol: None,
        };
        assert_eq!(f.__repr__(), "Frame(0x5000 libfoo.so)");
    }

    #[test]
    fn allocation_repr() {
        let a = PyAllocation {
            address: 0x1000,
            size: 0x100,
            live: true,
            function: "malloc".into(),
            thread: 2,
            backtrace: vec![],
            freed: None,
        };
        assert_eq!(
            a.__repr__(),
            "Allocation(0x1000 size=0x100 malloc thread=2 live=true)"
        );
    }
}
