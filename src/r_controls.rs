use crate::reply_overflow::{ReplyOverflowBehavior, ReplyOverflowSettings};
use harp::Result;
use harp::object::RObject;
use libr::SEXP;

#[harp::register]
pub extern "C-unwind" fn mcp_console_clear_pending_input() -> Result<SEXP> {
    let _ = crate::r_session::clear_pending_input();
    unsafe { Ok(libr::R_NilValue) }
}

#[harp::register]
pub extern "C-unwind" fn mcp_console_reply_overflow_update(
    behavior: SEXP,
    text_preview_bytes: SEXP,
    text_spill_bytes: SEXP,
    images_preview_count: SEXP,
    images_spill_count: SEXP,
    retention_max_dirs: SEXP,
) -> Result<SEXP> {
    let settings = read_reply_overflow_settings(
        behavior,
        text_preview_bytes,
        text_spill_bytes,
        images_preview_count,
        images_spill_count,
        retention_max_dirs,
    );
    match settings {
        Ok(settings) => crate::ipc::emit_reply_overflow_settings(settings),
        Err(err) => eprintln!("mcp-console reply overflow update error: {err}"),
    }
    unsafe { Ok(libr::R_NilValue) }
}

fn read_reply_overflow_settings(
    behavior: SEXP,
    text_preview_bytes: SEXP,
    text_spill_bytes: SEXP,
    images_preview_count: SEXP,
    images_spill_count: SEXP,
    retention_max_dirs: SEXP,
) -> std::result::Result<ReplyOverflowSettings, String> {
    let behavior = String::try_from(RObject::view(behavior))
        .map_err(|err| format!("invalid behavior: {err}"))?;
    let text_preview_bytes = read_u64("text preview bytes", text_preview_bytes)?;
    let text_spill_bytes = read_u64("text spill bytes", text_spill_bytes)?;
    let images_preview_count = read_usize("image preview count", images_preview_count)?;
    let images_spill_count = read_usize("image spill count", images_spill_count)?;
    let retention_max_dirs = read_usize("retention max dirs", retention_max_dirs)?;

    let settings = ReplyOverflowSettings {
        behavior: ReplyOverflowBehavior::parse(&behavior)?,
        text: crate::reply_overflow::ReplyOverflowTextSettings {
            preview_bytes: text_preview_bytes,
            spill_bytes: text_spill_bytes,
        },
        images: crate::reply_overflow::ReplyOverflowImageSettings {
            preview_count: images_preview_count,
            spill_count: images_spill_count,
        },
        retention: crate::reply_overflow::ReplyOverflowRetentionSettings {
            max_dirs: retention_max_dirs,
        },
    };
    settings.validate()?;
    Ok(settings)
}

fn read_u64(label: &str, value: SEXP) -> std::result::Result<u64, String> {
    let value =
        i64::try_from(RObject::view(value)).map_err(|err| format!("invalid {label}: {err}"))?;
    u64::try_from(value).map_err(|_| format!("{label} must be non-negative"))
}

fn read_usize(label: &str, value: SEXP) -> std::result::Result<usize, String> {
    let value =
        i64::try_from(RObject::view(value)).map_err(|err| format!("invalid {label}: {err}"))?;
    usize::try_from(value).map_err(|_| format!("{label} must be non-negative"))
}
