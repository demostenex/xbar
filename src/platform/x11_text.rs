use std::error::Error;
use std::ffi::{c_int, CString};
use std::fmt::Arguments;
use std::io::Write;
use std::os::fd::RawFd;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ui::style::{FontMetrics, TextMeasurer};
use x11::{xft, xlib, xrender};

static XFT_DRAW_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn xft_trace_enabled() -> bool {
    std::env::var_os("XBAR_TRACE_XFT").is_some()
}

fn trace_xft(args: Arguments<'_>) {
    if xft_trace_enabled() {
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        let _ = writeln!(stderr, "xbar xft: {args}");
        let _ = stderr.flush();
    }
}

struct DrawResource {
    sequence: u64,
    role: &'static str,
    drawable: u32,
    draw: *mut xft::XftDraw,
    picture: u64,
}

pub struct X11Text {
    display: *mut xlib::Display,
    bar_font: *mut xft::XftFont,
    popup_font: *mut xft::XftFont,
    status_icon_font: *mut xft::XftFont,
    draw: Option<DrawResource>,
    font_name: CString,
    metrics: FontMetrics,
    popup_metrics: FontMetrics,
    status_icon_metrics: FontMetrics,
    status_icon_font_name: CString,
}

impl X11Text {
    pub fn open() -> Result<Self, Box<dyn Error>> {
        let display = unsafe { xlib::XOpenDisplay(ptr::null()) };
        if display.is_null() {
            return Err("XOpenDisplay failed".into());
        }
        let font_name = CString::new("MesloLGS Nerd Font Mono:style=Bold:size=9")?;
        let popup_font_name = CString::new("MesloLGS Nerd Font Mono:size=12")?;
        let status_icon_font_name = CString::new("MesloLGS Nerd Font Mono:size=13")?;
        let screen = unsafe { xlib::XDefaultScreen(display) };
        let bar_font = unsafe { xft::XftFontOpenName(display, screen, font_name.as_ptr()) };
        let popup_font = unsafe { xft::XftFontOpenName(display, screen, popup_font_name.as_ptr()) };
        let status_icon_font =
            unsafe { xft::XftFontOpenName(display, screen, status_icon_font_name.as_ptr()) };
        if bar_font.is_null() || popup_font.is_null() || status_icon_font.is_null() {
            unsafe { xlib::XCloseDisplay(display) };
            return Err("XftFontOpenName failed for MesloLGS Nerd Font Mono".into());
        }
        Ok(Self {
            display,
            bar_font,
            popup_font,
            status_icon_font,
            draw: None,
            font_name,
            metrics: FontMetrics {
                ascent: unsafe { (*bar_font).ascent as i16 },
                descent: unsafe { (*bar_font).descent as i16 },
            },
            popup_metrics: FontMetrics {
                ascent: unsafe { (*popup_font).ascent as i16 },
                descent: unsafe { (*popup_font).descent as i16 },
            },
            status_icon_metrics: FontMetrics {
                ascent: unsafe { (*status_icon_font).ascent as i16 },
                descent: unsafe { (*status_icon_font).descent as i16 },
            },
            status_icon_font_name,
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        unsafe { xlib::XConnectionNumber(self.display) }
    }

    pub fn font_name(&self) -> &str {
        self.font_name.to_str().unwrap_or("unknown")
    }

    pub fn popup_font_name(&self) -> &str {
        "MesloLGS Nerd Font Mono:size=12"
    }

    pub fn popup_metrics(&self) -> FontMetrics {
        self.popup_metrics
    }

    pub fn status_icon_font_name(&self) -> &str {
        self.status_icon_font_name.to_str().unwrap_or("unknown")
    }
    pub fn status_icon_metrics(&self) -> FontMetrics {
        self.status_icon_metrics
    }
    pub fn draw_status_icon_utf8(
        &self,
        text: &str,
        x: i32,
        y: i32,
        color: u32,
    ) -> Result<(), Box<dyn Error>> {
        self.draw_with_font(self.status_icon_font, text, x, y, color)
    }

    pub fn popup_baseline(&self, height: u16) -> i16 {
        ((height as i16 - self.popup_metrics.descent + self.popup_metrics.ascent) / 2).max(1)
    }

    pub fn prepare_drawable(
        &mut self,
        role: &'static str,
        drawable: u32,
        drawable_visual: u32,
        drawable_depth: u8,
    ) -> Result<(), Box<dyn Error>> {
        if self
            .draw
            .as_ref()
            .is_some_and(|resource| resource.drawable == drawable)
        {
            return Ok(());
        }
        let screen = unsafe { xlib::XDefaultScreen(self.display) };
        let visual = unsafe { xlib::XDefaultVisual(self.display, screen) };
        let default_visual = unsafe { xlib::XVisualIDFromVisual(visual) };
        let default_depth = unsafe { xlib::XDefaultDepth(self.display, screen) };
        if drawable_visual as u64 != default_visual || drawable_depth as c_int != default_depth {
            return Err(format!(
                "Xft visual mismatch: drawable visual=0x{drawable_visual:x} depth={drawable_depth}, default visual=0x{default_visual:x} depth={default_depth}"
            )
            .into());
        }
        let draw = unsafe {
            xft::XftDrawCreate(
                self.display,
                drawable as u64,
                visual,
                xlib::XDefaultColormap(self.display, screen),
            )
        };
        if draw.is_null() {
            return Err("XftDrawCreate failed".into());
        }
        let tracing = xft_trace_enabled();
        let sequence = if tracing {
            XFT_DRAW_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        let picture = if tracing {
            unsafe { xft::XftDrawPicture(draw) as u64 }
        } else {
            0
        };
        trace_xft(format_args!(
            "XFT_DRAW_CREATE seq={sequence} role={role} draw_ptr={draw:p} drawable=0x{drawable:x} picture=0x{picture:x}"
        ));
        let replacement = DrawResource {
            sequence,
            role,
            drawable,
            draw,
            picture,
        };
        if let Some(old) = self.draw.replace(replacement) {
            trace_xft(format_args!(
                "XFT_DRAW_REBIND old_seq={} new_seq={sequence} old_role={} old_draw_ptr={:p} old_drawable=0x{:x} old_picture=0x{:x} new_role={role} new_draw_ptr={:p} new_drawable=0x{drawable:x} new_picture=0x{picture:x}",
                old.sequence, old.role, old.draw, old.drawable, old.picture, draw
            ));
            trace_xft(format_args!(
                "XFT_DRAW_DESTROY seq={} role={} draw_ptr={:p} drawable=0x{:x} picture=0x{:x} reason=replace",
                old.sequence, old.role, old.draw, old.drawable, old.picture
            ));
            unsafe {
                xft::XftDrawDestroy(old.draw);
                xlib::XSync(self.display, 0);
            }
        }
        Ok(())
    }

    pub fn release_drawable(&mut self, drawable: u32) {
        if self
            .draw
            .as_ref()
            .is_some_and(|resource| resource.drawable == drawable)
        {
            if let Some(resource) = self.draw.take() {
                trace_xft(format_args!(
                    "XFT_DRAW_DESTROY seq={} role={} draw_ptr={:p} drawable=0x{:x} picture=0x{:x} reason=release",
                    resource.sequence, resource.role, resource.draw, resource.drawable, resource.picture
                ));
                unsafe {
                    xft::XftDrawDestroy(resource.draw);
                    xlib::XSync(self.display, 0);
                }
            }
        }
    }

    pub fn draw_utf8(&self, text: &str, x: i32, y: i32, color: u32) -> Result<(), Box<dyn Error>> {
        self.draw_with_font(self.bar_font, text, x, y, color)
    }

    pub fn draw_popup_utf8(
        &self,
        text: &str,
        x: i32,
        y: i32,
        color: u32,
    ) -> Result<(), Box<dyn Error>> {
        self.draw_with_font(self.popup_font, text, x, y, color)
    }

    fn draw_with_font(
        &self,
        font: *mut xft::XftFont,
        text: &str,
        x: i32,
        y: i32,
        color: u32,
    ) -> Result<(), Box<dyn Error>> {
        let Some(resource) = self.draw.as_ref() else {
            return Err("Xft drawable is not initialized".into());
        };
        let draw = resource.draw;
        let text = CString::new(text)?;
        let mut xft_color = unsafe { std::mem::zeroed::<xft::XftColor>() };
        let value = xrender::XRenderColor {
            red: ((color >> 16) as u16) * 257,
            green: (((color >> 8) & 0xff) as u16) * 257,
            blue: ((color & 0xff) as u16) * 257,
            alpha: u16::MAX,
        };
        let screen = unsafe { xlib::XDefaultScreen(self.display) };
        let visual = unsafe { xlib::XDefaultVisual(self.display, screen) };
        let colormap = unsafe { xlib::XDefaultColormap(self.display, screen) };
        if unsafe {
            xft::XftColorAllocValue(self.display, visual, colormap, &value, &mut xft_color)
        } == 0
        {
            return Err("XftColorAllocValue failed".into());
        }
        unsafe {
            xft::XftDrawStringUtf8(
                draw,
                &xft_color,
                font,
                x,
                y,
                text.as_ptr() as *const u8,
                text.as_bytes().len() as c_int,
            );
            xft::XftColorFree(self.display, visual, colormap, &mut xft_color);
        }
        Ok(())
    }

    pub fn flush(&self) {
        unsafe {
            xlib::XFlush(self.display);
        }
    }
    pub fn measure_width(&self, text: &str) -> u16 {
        self.measure_with_font(self.bar_font, text)
    }

    pub fn measure_popup_width(&self, text: &str) -> u16 {
        self.measure_with_font(self.popup_font, text)
    }
    pub fn measure_status_icon_width(&self, text: &str) -> u16 {
        self.measure_with_font(self.status_icon_font, text)
    }

    fn measure_with_font(&self, font: *mut xft::XftFont, text: &str) -> u16 {
        let text = CString::new(text).ok();
        let Some(text) = text else {
            return 0;
        };
        let mut extents = unsafe { std::mem::zeroed::<xrender::XGlyphInfo>() };
        unsafe {
            xft::XftTextExtentsUtf8(
                self.display,
                font,
                text.as_ptr() as *const u8,
                text.as_bytes().len() as c_int,
                &mut extents,
            );
        }
        extents.xOff.max(0) as u16
    }
}

impl Drop for X11Text {
    fn drop(&mut self) {
        unsafe {
            if let Some(resource) = self.draw.take() {
                trace_xft(format_args!(
                    "XFT_DRAW_DESTROY seq={} role={} draw_ptr={:p} drawable=0x{:x} picture=0x{:x} reason=drop",
                    resource.sequence, resource.role, resource.draw, resource.drawable, resource.picture
                ));
                xft::XftDrawDestroy(resource.draw);
            }
            xft::XftFontClose(self.display, self.popup_font);
            xft::XftFontClose(self.display, self.status_icon_font);
            xft::XftFontClose(self.display, self.bar_font);
            xlib::XCloseDisplay(self.display);
        }
    }
}

// X11Text is only accessed by xbar's single main thread.
unsafe impl Send for X11Text {}

impl TextMeasurer for X11Text {
    fn measure_width(&self, text: &str) -> u16 {
        self.measure_width(text)
    }
    fn metrics(&self) -> FontMetrics {
        self.metrics
    }
    fn measure_status_icon_width(&self, text: &str) -> u16 {
        self.measure_status_icon_width(text)
    }
}
