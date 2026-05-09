use std::ffi::{CStr, CString, c_char, c_int, c_long, c_void};
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;

use libloading::Library;

pub const PY_FILE_INPUT: c_int = 257;
const PYTHON_API_VERSION: c_int = 1013;
const METH_VARARGS: c_int = 0x0001;

#[cfg(any(target_pointer_width = "64", target_pointer_width = "32"))]
type PySsizeT = isize;

#[repr(C)]
pub struct PyTypeObject {
    _private: [u8; 0],
}

#[repr(C)]
pub struct PyObject {
    ob_refcnt: PySsizeT,
    ob_type: *mut PyTypeObject,
}

pub enum PyThreadState {}

pub type PyCFunction = unsafe extern "C" fn(*mut PyObject, *mut PyObject) -> *mut PyObject;
pub type PyGILStateState = c_int;
pub type PyOsInputHookCallback = unsafe extern "C" fn() -> c_int;

#[repr(C)]
struct PyMethodDef {
    ml_name: *const c_char,
    ml_meth: Option<PyCFunction>,
    ml_flags: c_int,
    ml_doc: *const c_char,
}

#[repr(C)]
struct PyModuleDefBase {
    ob_base: PyObject,
    m_init: Option<unsafe extern "C" fn() -> *mut PyObject>,
    m_index: PySsizeT,
    m_copy: *mut PyObject,
}

#[repr(C)]
struct PyModuleDef {
    m_base: PyModuleDefBase,
    m_name: *const c_char,
    m_doc: *const c_char,
    m_size: PySsizeT,
    m_methods: *mut PyMethodDef,
    m_reload: Option<unsafe extern "C" fn(*mut PyObject) -> c_int>,
    m_traverse: Option<unsafe extern "C" fn(*mut PyObject, *mut std::ffi::c_void) -> c_int>,
    m_clear: Option<unsafe extern "C" fn(*mut PyObject) -> c_int>,
    m_free: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
}

pub struct ModuleMethod {
    pub name: &'static str,
    pub function: PyCFunction,
}

pub struct PythonApi {
    _library: Library,
    pub py_initialize_ex: unsafe extern "C" fn(c_int),
    pub py_is_initialized: unsafe extern "C" fn() -> c_int,
    pub py_eval_save_thread: unsafe extern "C" fn() -> *mut PyThreadState,
    pub py_gil_state_ensure: unsafe extern "C" fn() -> PyGILStateState,
    pub py_gil_state_release: unsafe extern "C" fn(PyGILStateState),
    pub py_import_append_inittab:
        unsafe extern "C" fn(*const c_char, unsafe extern "C" fn() -> *mut PyObject) -> c_int,
    py_module_create2: unsafe extern "C" fn(*mut PyModuleDef, c_int) -> *mut PyObject,
    pub py_import_import_module: unsafe extern "C" fn(*const c_char) -> *mut PyObject,
    pub py_module_get_dict: unsafe extern "C" fn(*mut PyObject) -> *mut PyObject,
    pub py_run_string_flags: unsafe extern "C" fn(
        *const c_char,
        c_int,
        *mut PyObject,
        *mut PyObject,
        *mut std::ffi::c_void,
    ) -> *mut PyObject,
    pub py_run_interactive_one_flags:
        unsafe extern "C" fn(*mut libc::FILE, *const c_char, *mut c_void) -> c_int,
    pub py_os_readline:
        unsafe extern "C" fn(*mut libc::FILE, *mut libc::FILE, *const c_char) -> *mut c_char,
    pub py_object_get_attr_string:
        unsafe extern "C" fn(*mut PyObject, *const c_char) -> *mut PyObject,
    pub py_object_call_object: unsafe extern "C" fn(*mut PyObject, *mut PyObject) -> *mut PyObject,
    pub py_object_is_true: unsafe extern "C" fn(*mut PyObject) -> c_int,
    pub py_tuple_size: unsafe extern "C" fn(*mut PyObject) -> PySsizeT,
    pub py_tuple_get_item: unsafe extern "C" fn(*mut PyObject, PySsizeT) -> *mut PyObject,
    pub py_unicode_from_string_and_size:
        unsafe extern "C" fn(*const c_char, PySsizeT) -> *mut PyObject,
    pub py_unicode_as_utf8_and_size:
        unsafe extern "C" fn(*mut PyObject, *mut PySsizeT) -> *const c_char,
    pub py_long_from_long: unsafe extern "C" fn(c_long) -> *mut PyObject,
    pub py_bool_from_long: unsafe extern "C" fn(c_long) -> *mut PyObject,
    pub py_build_value: unsafe extern "C" fn(*const c_char, ...) -> *mut PyObject,
    pub py_mem_free: unsafe extern "C" fn(*mut c_void),
    pub py_dec_ref: unsafe extern "C" fn(*mut PyObject),
    pub py_err_print: unsafe extern "C" fn(),
    pub py_err_clear: unsafe extern "C" fn(),
    pub py_err_set_interrupt: unsafe extern "C" fn(),
    pub py_err_set_string: unsafe extern "C" fn(*mut PyObject, *const c_char),
}

static PYTHON_API: OnceLock<PythonApi> = OnceLock::new();

impl PythonApi {
    pub fn initialize(lib_path: &Path) -> Result<&'static Self, String> {
        if let Some(api) = PYTHON_API.get() {
            return Ok(api);
        }

        let api = unsafe { Self::load(lib_path)? };
        PYTHON_API
            .set(api)
            .map_err(|_| "Python C API was initialized concurrently".to_string())?;
        Ok(PYTHON_API.get().expect("Python C API was just initialized"))
    }

    pub fn global() -> &'static Self {
        PYTHON_API.get().expect("Python C API was not initialized")
    }

    pub fn try_global() -> Option<&'static Self> {
        PYTHON_API.get()
    }

    unsafe fn load(lib_path: &Path) -> Result<Self, String> {
        let library = load_library_global(lib_path)?;
        let api = Self {
            py_initialize_ex: unsafe { load_symbol(&library, b"Py_InitializeEx\0")? },
            py_is_initialized: unsafe { load_symbol(&library, b"Py_IsInitialized\0")? },
            py_eval_save_thread: unsafe { load_symbol(&library, b"PyEval_SaveThread\0")? },
            py_gil_state_ensure: unsafe { load_symbol(&library, b"PyGILState_Ensure\0")? },
            py_gil_state_release: unsafe { load_symbol(&library, b"PyGILState_Release\0")? },
            py_import_append_inittab: unsafe {
                load_symbol(&library, b"PyImport_AppendInittab\0")?
            },
            py_module_create2: unsafe { load_symbol(&library, b"PyModule_Create2\0")? },
            py_import_import_module: unsafe { load_symbol(&library, b"PyImport_ImportModule\0")? },
            py_module_get_dict: unsafe { load_symbol(&library, b"PyModule_GetDict\0")? },
            py_run_string_flags: unsafe { load_symbol(&library, b"PyRun_StringFlags\0")? },
            py_run_interactive_one_flags: unsafe {
                load_symbol(&library, b"PyRun_InteractiveOneFlags\0")?
            },
            py_os_readline: unsafe { load_symbol(&library, b"PyOS_Readline\0")? },
            py_object_get_attr_string: unsafe {
                load_symbol(&library, b"PyObject_GetAttrString\0")?
            },
            py_object_call_object: unsafe { load_symbol(&library, b"PyObject_CallObject\0")? },
            py_object_is_true: unsafe { load_symbol(&library, b"PyObject_IsTrue\0")? },
            py_tuple_size: unsafe { load_symbol(&library, b"PyTuple_Size\0")? },
            py_tuple_get_item: unsafe { load_symbol(&library, b"PyTuple_GetItem\0")? },
            py_unicode_from_string_and_size: unsafe {
                load_symbol(&library, b"PyUnicode_FromStringAndSize\0")?
            },
            py_unicode_as_utf8_and_size: unsafe {
                load_symbol(&library, b"PyUnicode_AsUTF8AndSize\0")?
            },
            py_long_from_long: unsafe { load_symbol(&library, b"PyLong_FromLong\0")? },
            py_bool_from_long: unsafe { load_symbol(&library, b"PyBool_FromLong\0")? },
            py_build_value: unsafe { load_symbol(&library, b"Py_BuildValue\0")? },
            py_mem_free: unsafe { load_symbol(&library, b"PyMem_Free\0")? },
            py_dec_ref: unsafe { load_symbol(&library, b"Py_DecRef\0")? },
            py_err_print: unsafe { load_symbol(&library, b"PyErr_Print\0")? },
            py_err_clear: unsafe { load_symbol(&library, b"PyErr_Clear\0")? },
            py_err_set_interrupt: unsafe { load_symbol(&library, b"PyErr_SetInterrupt\0")? },
            py_err_set_string: unsafe { load_symbol(&library, b"PyErr_SetString\0")? },
            _library: library,
        };
        Ok(api)
    }

    pub fn create_module(&self, name: &str, methods: &[ModuleMethod]) -> *mut PyObject {
        let mut method_defs = Vec::with_capacity(methods.len() + 1);
        for method in methods {
            method_defs.push(PyMethodDef {
                ml_name: leak_c_string(method.name),
                ml_meth: Some(method.function),
                ml_flags: METH_VARARGS,
                ml_doc: ptr::null(),
            });
        }
        method_defs.push(PyMethodDef {
            ml_name: ptr::null(),
            ml_meth: None,
            ml_flags: 0,
            ml_doc: ptr::null(),
        });
        let method_defs = Box::leak(method_defs.into_boxed_slice());
        let module_def = Box::leak(Box::new(PyModuleDef {
            m_base: PyModuleDefBase {
                ob_base: PyObject {
                    ob_refcnt: 1,
                    ob_type: ptr::null_mut(),
                },
                m_init: None,
                m_index: 0,
                m_copy: ptr::null_mut(),
            },
            m_name: leak_c_string(name),
            m_doc: ptr::null(),
            m_size: -1,
            m_methods: method_defs.as_mut_ptr(),
            m_reload: None,
            m_traverse: None,
            m_clear: None,
            m_free: None,
        }));
        unsafe { (self.py_module_create2)(module_def, PYTHON_API_VERSION) }
    }

    pub fn import_module(&self, name: &str) -> Result<PyPtr, String> {
        let name = CString::new(name).map_err(|_| "module name contains NUL".to_string())?;
        let ptr = unsafe { (self.py_import_import_module)(name.as_ptr()) };
        PyPtr::from_owned(ptr, "module import failed")
    }

    pub fn get_attr_string(&self, object: *mut PyObject, name: &str) -> Result<PyPtr, String> {
        let name = CString::new(name).map_err(|_| "attribute name contains NUL".to_string())?;
        let ptr = unsafe { (self.py_object_get_attr_string)(object, name.as_ptr()) };
        PyPtr::from_owned(ptr, "attribute lookup failed")
    }

    pub fn run_code(&self, code: &str, globals: *mut PyObject) -> Result<(), String> {
        let code = CString::new(code).map_err(|_| "Python source contains NUL".to_string())?;
        let result = unsafe {
            (self.py_run_string_flags)(
                code.as_ptr(),
                PY_FILE_INPUT,
                globals,
                globals,
                ptr::null_mut(),
            )
        };
        let result = PyPtr::from_owned(result, "Python code execution failed")?;
        drop(result);
        Ok(())
    }

    pub fn unicode(&self, value: &str) -> Result<PyPtr, String> {
        let ptr = unsafe {
            (self.py_unicode_from_string_and_size)(
                value.as_ptr().cast::<c_char>(),
                value.len() as PySsizeT,
            )
        };
        PyPtr::from_owned(ptr, "failed to allocate Python string")
    }

    pub fn unicode_arg(&self, args: *mut PyObject, index: PySsizeT) -> Option<String> {
        let item = unsafe { (self.py_tuple_get_item)(args, index) };
        if item.is_null() {
            return None;
        }
        self.unicode_to_string(item)
    }

    pub fn unicode_to_string(&self, object: *mut PyObject) -> Option<String> {
        let mut size: PySsizeT = 0;
        let ptr = unsafe { (self.py_unicode_as_utf8_and_size)(object, &mut size) };
        if ptr.is_null() || size < 0 {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size as usize) };
        Some(String::from_utf8_lossy(bytes).to_string())
    }

    pub fn tuple_size(&self, args: *mut PyObject) -> PySsizeT {
        unsafe { (self.py_tuple_size)(args) }
    }

    pub fn bool_result(&self, value: bool) -> *mut PyObject {
        unsafe { (self.py_bool_from_long)(if value { 1 } else { 0 }) }
    }

    pub fn long_result(&self, value: c_long) -> *mut PyObject {
        unsafe { (self.py_long_from_long)(value) }
    }

    pub fn none(&self) -> *mut PyObject {
        unsafe { (self.py_build_value)(c"".as_ptr()) }
    }

    pub fn set_runtime_error(&self, exception: *mut PyObject, message: &str) {
        let message =
            CString::new(message).expect("internal Python error message must not contain NUL");
        unsafe { (self.py_err_set_string)(exception, message.as_ptr()) };
    }

    pub fn install_input_hook(&self, callback: PyOsInputHookCallback) -> Result<(), String> {
        let symbol = unsafe {
            self._library
                .get::<*mut PyOsInputHookCallback>(b"PyOS_InputHook\0")
                .map_err(|err| format!("failed to load PyOS_InputHook: {err}"))?
        };
        unsafe {
            **symbol = callback;
        }
        Ok(())
    }

    pub fn set_interactive_flags(&self) -> Result<(), String> {
        unsafe {
            **self
                ._library
                .get::<*mut c_int>(b"Py_InteractiveFlag\0")
                .map_err(|err| format!("failed to load Py_InteractiveFlag: {err}"))? = 1;
            **self
                ._library
                .get::<*mut c_int>(b"Py_InspectFlag\0")
                .map_err(|err| format!("failed to load Py_InspectFlag: {err}"))? = 1;
        }
        Ok(())
    }

    pub fn print_error(&self) {
        unsafe { (self.py_err_print)() };
    }

    pub fn clear_error(&self) {
        unsafe { (self.py_err_clear)() };
    }
}

pub struct PyPtr {
    ptr: *mut PyObject,
}

impl PyPtr {
    pub fn from_owned(ptr: *mut PyObject, context: &str) -> Result<Self, String> {
        if ptr.is_null() {
            Err(context.to_string())
        } else {
            Ok(Self { ptr })
        }
    }

    pub fn as_ptr(&self) -> *mut PyObject {
        self.ptr
    }

    pub fn into_raw(mut self) -> *mut PyObject {
        let ptr = self.ptr;
        self.ptr = ptr::null_mut();
        ptr
    }
}

impl Drop for PyPtr {
    fn drop(&mut self) {
        if !self.ptr.is_null()
            && let Some(api) = PYTHON_API.get()
        {
            unsafe { (api.py_dec_ref)(self.ptr) };
        }
    }
}

pub struct GilGuard {
    api: &'static PythonApi,
    state: PyGILStateState,
}

impl GilGuard {
    pub fn acquire() -> Self {
        let api = PythonApi::global();
        let state = unsafe { (api.py_gil_state_ensure)() };
        Self { api, state }
    }
}

impl Drop for GilGuard {
    fn drop(&mut self) {
        unsafe { (self.api.py_gil_state_release)(self.state) };
    }
}

#[cfg(unix)]
fn load_library_global(path: &Path) -> Result<Library, String> {
    let os_lib = unsafe {
        libloading::os::unix::Library::open(
            Some(path.as_os_str()),
            libc::RTLD_NOW | libc::RTLD_GLOBAL,
        )
    }
    .map_err(|err| format!("failed to load Python library {}: {err}", path.display()))?;
    Ok(os_lib.into())
}

#[cfg(not(unix))]
fn load_library_global(path: &Path) -> Result<Library, String> {
    unsafe { Library::new(path) }
        .map_err(|err| format!("failed to load Python library {}: {err}", path.display()))
}

unsafe fn load_symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, String> {
    let symbol = unsafe {
        library
            .get::<T>(name)
            .map_err(|err| format!("failed to load Python symbol {}: {err}", symbol_name(name)))?
    };
    Ok(*symbol)
}

fn symbol_name(name: &[u8]) -> String {
    CStr::from_bytes_with_nul(name)
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|_| String::from_utf8_lossy(name).to_string())
}

fn leak_c_string(value: &str) -> *const c_char {
    CString::new(value)
        .expect("internal Python C string must not contain NUL")
        .into_raw()
}
