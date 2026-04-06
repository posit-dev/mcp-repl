use harp::object::RObject;
use harp::protect::RProtect;
use libr::SEXP;

#[cfg_attr(windows, allow(clippy::result_large_err))]
#[harp::register]
pub extern "C-unwind" fn mcp_repl_htmd_file_to_markdown(path: SEXP) -> harp::Result<SEXP> {
    let path = String::try_from(RObject::view(path))?;
    if path.trim().is_empty() {
        return unsafe { Ok(libr::R_NilValue) };
    }

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return unsafe { Ok(libr::R_NilValue) },
    };
    let html = String::from_utf8_lossy(&bytes);

    let md = match crate::html_to_markdown::convert(&html) {
        Ok(md) => md,
        Err(_) => return unsafe { Ok(libr::R_NilValue) },
    };

    unsafe {
        let mut protect = RProtect::new();
        Ok(harp::r_string!(md, &mut protect))
    }
}

#[cfg_attr(windows, allow(clippy::result_large_err))]
#[harp::register]
pub extern "C-unwind" fn mcp_repl_htmd_html_to_markdown(html: SEXP) -> harp::Result<SEXP> {
    let html = String::try_from(RObject::view(html))?;
    if html.trim().is_empty() {
        return unsafe { Ok(libr::R_NilValue) };
    }

    let md = match crate::html_to_markdown::convert(&html) {
        Ok(md) => md,
        Err(_) => return unsafe { Ok(libr::R_NilValue) },
    };

    unsafe {
        let mut protect = RProtect::new();
        Ok(harp::r_string!(md, &mut protect))
    }
}
