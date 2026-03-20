use harp::object::RObject;
use libr::SEXP;

#[harp::register]
pub extern "C-unwind" fn mcp_repl_plot_emit(
    id: SEXP,
    data: SEXP,
    mime_type: SEXP,
    is_new: SEXP,
) -> harp::Result<SEXP> {
    let id = match String::try_from(RObject::view(id)) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("mcp-repl plot emit error: invalid plot id: {err}");
            return unsafe { Ok(libr::R_NilValue) };
        }
    };
    let data_obj = RObject::view(data);
    let bytes: Vec<u8> = match (&data_obj).try_into() {
        Ok(value) => value,
        Err(err) => {
            eprintln!("mcp-repl plot emit error: invalid plot data: {err}");
            return unsafe { Ok(libr::R_NilValue) };
        }
    };
    let mime_type = match String::try_from(RObject::view(mime_type)) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("mcp-repl plot emit error: invalid mime type: {err}");
            return unsafe { Ok(libr::R_NilValue) };
        }
    };
    let is_new = bool::try_from(RObject::view(is_new)).unwrap_or(false);

    if let Err(err) = crate::r_session::push_plot_image(id, bytes, mime_type, is_new) {
        eprintln!("mcp-repl plot emit error: {err}");
    }

    unsafe { Ok(libr::R_NilValue) }
}
