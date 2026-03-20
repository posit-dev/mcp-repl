use harp::Result;
use libr::SEXP;

#[harp::register]
pub extern "C-unwind" fn mcp_repl_clear_pending_input() -> Result<SEXP> {
    let _ = crate::r_session::clear_pending_input();
    unsafe { Ok(libr::R_NilValue) }
}
