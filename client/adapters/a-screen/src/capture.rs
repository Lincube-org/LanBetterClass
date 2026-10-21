//! 抓屏后端。按平台选路，用 `params.capture` 选：
//!
//! Windows：
//! - `gdi`  —— `GetDC` + `BitBlt` + `GetDIBits`。哪儿都能跑：RDP 远程协助、虚机、
//!   基础显示驱动下都有画面，代价是只能轮询（每 poll 拷一次全屏，没人告诉你"变了"）。
//! - `dxgi` —— `IDXGIOutputDuplication` 桌面复制。合成器直接交出帧，没有新帧时
//!   `AcquireNextFrame` 立刻超时返回，几乎不花钱；但 RDP 会话与一些虚机里拿不到
//!   复制接口，分辨率或模式一切换还会 `ACCESS_LOST`，必须重建。
//! - `auto` —— 先试 DXGI，拿不到就退回 GDI，**并把"用的哪条 + 为什么退"写进 session.open**。
//!
//! Linux：
//! - `x11`  —— `GetImage` 轮询。有合成器时抓 Composite overlay（根窗口此时只有背景），
//!   没有合成器时抓根窗口。纯 Rust `x11rb`，不链 libxcb；多屏是拼接画面，
//!   `monitor` 只支持 0（整屏）。Wayland 会话明确拒绝：XWayland 的根窗口不是整块桌面，
//!   抓到的大概率是黑屏或局部，宁可在健康表里缺席，也不把"没采到"伪装成一节黑屏课。
//! - `auto` —— 走 X11。
//! - PipeWire / Wayland 抓屏尚未实现。
//!
//! 所有后端的产物都是同一种 RGB 帧，所以上层（变化检测、落盘、事件）完全不区分来源——
//! 与 a-audio 的设备/回放路径同一个道理。
//!
//! 没有可用后端时一律明确拒绝：那个源在这台机器上就是不存在，让它在健康表里缺席，
//! 而不是假装采到一张黑屏。

pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// 行优先 RGB，长度 `width * height * 3`。
    pub rgb: Vec<u8>,
}

pub trait Backend {
    /// 抓一帧。`Ok(None)` 只表达一件事：这一刻没有新画面（只有 DXGI 会说这句话）。
    fn grab(&mut self) -> Result<Option<Frame>, String>;
    /// 进 `session.open` 的自述：用了哪条路、哪台显示器、多大、重建过几次。
    fn describe(&self) -> serde_json::Value;
    /// 后端自己出过几次事（GDI 调用失败、DXGI ACCESS_LOST 重建）。
    /// 它必须进数据而不是只进 stderr：如果一节课的帧全是在重建途中抢来的，
    /// "老师什么时候翻的页"就不可信，课后复盘得能看到这个前提。
    fn errors(&self) -> usize;
}

pub struct Opened {
    pub backend: Box<dyn Backend>,
    /// `auto` 退到 GDI 时，DXGI 那句失败原因。丢掉它就等于把兼容性妥协藏起来。
    pub fallback: Option<String>,
}

/// 观察端那个下拉框必须和这份清单一致（`.github/workflows/agent.yml` 有文本守卫钉它）。
/// 清单是"跨平台已知的全部取值"，具体哪条能在本机跑由 `open()` 按平台判断并明确报错。
pub const BACKENDS: [&str; 4] = ["auto", "gdi", "dxgi", "x11"];

#[cfg(not(any(windows, target_os = "linux")))]
pub fn open(_want: &str, _monitor: usize) -> Result<Opened, String> {
    Err("本机既不是 Windows 也不是 Linux：没有可用的设备抓屏后端（只有 fixture 回放）".into())
}

#[cfg(windows)]
pub fn open(want: &str, monitor: usize) -> Result<Opened, String> {
    match want {
        "gdi" => return gdi::opened(monitor).map(|b| Opened { backend: b, fallback: None }),
        "dxgi" => return dxgi::opened(monitor).map(|b| Opened { backend: b, fallback: None }),
        "x11" => {
            return Err("x11 只适用于 Linux；Windows 上用 auto / gdi / dxgi".into());
        }
        "auto" => {}
        other => {
            // 未知取值不猜：写错的人要立刻知道，而不是"用另一条路安安静静采了一整节课"。
            return Err(format!("params.capture 只认 auto / gdi / dxgi / x11，收到 {other:?}"));
        }
    }
    match dxgi::opened(monitor) {
        Ok(b) => Ok(Opened { backend: b, fallback: None }),
        Err(e) => match gdi::opened(monitor) {
            Ok(b) => Ok(Opened { backend: b, fallback: Some(format!("DXGI 不可用，退回 GDI：{e}")) }),
            Err(e2) => Err(format!("两条路都不通：dxgi={e}；gdi={e2}")),
        },
    }
}

#[cfg(target_os = "linux")]
pub fn open(want: &str, monitor: usize) -> Result<Opened, String> {
    match want {
        "x11" | "auto" => x11::opened(monitor),
        "gdi" | "dxgi" => Err(format!("{want} 只适用于 Windows；Linux 上用 auto / x11")),
        other => {
            // 与 Windows 同一把尺：未知取值当场拒，不悄悄换一条路。
            Err(format!("params.capture 只认 auto / gdi / dxgi / x11，收到 {other:?}"))
        }
    }
}

/// DIB / 纹理的行都是 BGRA（小端 + BI_RGB 32bit 或 B8G8R8A8），转成 RGB 顺手丢掉 alpha。
/// 通道顺序反了不会报错，只会让一整节课的红蓝互换——而没人回看时这件事完全无声。
pub fn bgra_to_rgb(src: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; src.len() / 4 * 3];
    let mut j = 0usize;
    for p in src.chunks_exact(4) {
        out[j] = p[2];
        out[j + 1] = p[1];
        out[j + 2] = p[0];
        j += 3;
    }
    out
}

#[cfg(windows)]
mod gdi {
    use super::{bgra_to_rgb, Backend, Frame};
    use serde_json::json;
    use std::ffi::c_void;
    use windows::core::BOOL;
    // LPARAM 不在 windows::core 里：GDI 那边的回调签名把它放在 Win32::Foundation。
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
        GetDIBits, GetMonitorInfoW, HGDIOBJ, HDC, HMONITOR, MONITORENUMPROC, MONITORINFO, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;

    #[derive(Clone, Copy)]
    struct Rect {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    }

    pub struct Gdi {
        r: Rect,
        monitor: usize,
        primary: bool,
        dpi_aware: bool,
        errs: usize,
    }

    pub fn opened(monitor: usize) -> Result<Box<dyn Backend>, String> {
        Gdi::open(monitor).map(|g| Box::new(g) as Box<dyn Backend>)
    }

    unsafe extern "system" fn collect(
        hm: HMONITOR,
        _hdc: HDC,
        _rc: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<(Rect, bool)>);
        let mut mi = MONITORINFO::default();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hm, &mut mi).0 != 0 {
            let rc = mi.rcMonitor;
            // dwFlags 的第 1 位 = MONITORINFOF_PRIMARY。不引那个常量，避免为了一次
            // 位运算再拖一个 feature 进来。
            out.push((Rect { x: rc.left, y: rc.top, w: rc.right - rc.left, h: rc.bottom - rc.top }, mi.dwFlags & 1 != 0));
        }
        BOOL(1)
    }

    /// 主屏排第一，其余按系统枚举次序。这样 `monitor: 0` 在任何接法下都是"老师看的那块"，
    /// 不会随着插拔副屏悄悄把采集目标换掉——那是会毁掉一整节课的事。
    fn monitors() -> Vec<(Rect, bool)> {
        unsafe {
            let mut v: Vec<(Rect, bool)> = Vec::new();
            let cb: MONITORENUMPROC = Some(collect);
            EnumDisplayMonitors(None, None, cb, LPARAM(&mut v as *mut _ as isize));
            let (prim, rest): (Vec<_>, Vec<_>) = v.into_iter().partition(|(_, p)| *p);
            prim.into_iter().chain(rest).collect()
        }
    }

    impl Gdi {
        fn open(monitor: usize) -> Result<Self, String> {
            // 不设这个，150% 缩放的 4K 一体机上拿到的是系统拉伸过的图，
            // 课件上的小字在 blob 里永远读不出来。
            let dpi_aware = unsafe { SetProcessDPIAware() }.0 != 0;
            let ms = monitors();
            if ms.is_empty() {
                return Err("EnumDisplayMonitors 一个显示器都没枚举到（没有桌面？）。".into());
            }
            let idx = monitor.min(ms.len() - 1);
            let (r, primary) = ms[idx];
            if r.w <= 0 || r.h <= 0 {
                return Err(format!("显示器 {idx} 的矩形是 {}x{}，不可用", r.w, r.h));
            }
            Ok(Gdi { r, monitor: idx, primary, dpi_aware, errs: 0 })
        }
    }

    impl Backend for Gdi {
        fn grab(&mut self) -> Result<Option<Frame>, String> {
            let (w, h) = (self.r.w, self.r.h);
            let (pw, ph) = (w as usize, h as usize);
            unsafe {
                let screen = GetDC(None);
                if screen.is_invalid() {
                    self.errs += 1;
                    return Err("GetDC(None) 拿不到屏幕 DC".into());
                }
                let mem = CreateCompatibleDC(Some(screen));
                let bmp = CreateCompatibleBitmap(screen, w, h);
                let mut px = vec![0u8; pw * ph * 4];
                // BITMAPINFO 只 derive 了 Clone/Copy/Debug/PartialEq，没有 Default，
                // 所以整块清零再填头。它本就是 POD，zeroed 就是它的默认值。
                let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
                bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bmi.bmiHeader.biWidth = w;
                // 负高度 = 自上而下的 DIB，省掉一次整帧翻行。
                bmi.bmiHeader.biHeight = -h;
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 32;
                bmi.bmiHeader.biCompression = BI_RGB.0;
                let mut failure: Option<String> = None;
                let old = if mem.is_invalid() {
                    failure = Some("CreateCompatibleDC 失败".to_string());
                    HGDIOBJ(std::ptr::null_mut())
                } else if bmp.is_invalid() {
                    failure = Some("CreateCompatibleBitmap 失败".to_string());
                    HGDIOBJ(std::ptr::null_mut())
                } else {
                    SelectObject(mem, bmp.into())
                };
                if failure.is_none() {
                    if let Err(e) = BitBlt(mem, 0, 0, w, h, Some(screen), self.r.x, self.r.y, SRCCOPY) {
                        failure = Some(format!("BitBlt：{e}"));
                    }
                }
                if failure.is_none() {
                    // 返回的是"写了几行"，0 才是失败。
                    let got = GetDIBits(
                        mem,
                        bmp,
                        0,
                        h as u32,
                        Some(px.as_mut_ptr() as *mut c_void),
                        &mut bmi,
                        DIB_RGB_COLORS,
                    );
                    if got == 0 {
                        failure = Some("GetDIBits 一行都没写出来".to_string());
                    }
                }
                if !old.is_invalid() {
                    SelectObject(mem, old);
                }
                if !mem.is_invalid() {
                    let _ = DeleteDC(mem);
                }
                if !bmp.is_invalid() {
                    let _ = DeleteObject(bmp.into());
                }
                let _ = ReleaseDC(None, screen);
                if let Some(e) = failure {
                    self.errs += 1;
                    return Err(e);
                }
                Ok(Some(Frame { width: pw, height: ph, rgb: bgra_to_rgb(&px) }))
            }
        }

        fn describe(&self) -> serde_json::Value {
            json!({
                "backend": "gdi",
                "monitor": self.monitor,
                "primary": self.primary,
                "width": self.r.w.max(0) as u64,
                "height": self.r.h.max(0) as u64,
                "dpi_aware": self.dpi_aware,
            })
        }

        fn errors(&self) -> usize {
            self.errs
        }
    }
}

#[cfg(windows)]
mod dxgi {
    use super::{bgra_to_rgb, Backend, Frame};
    use serde_json::json;
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE,
        D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device,
        ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
        IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    };
    // 像素格式与采样描述在 Dxgi::Common 下，不在 Dxgi 本身；DXGI_OUTDUPL_DESC 也只给
    // 一个 ModeDesc，尺寸要从里面拿。
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };

    pub struct Dxgi {
        dup: IDXGIOutputDuplication,
        ctx: ID3D11DeviceContext,
        staging: ID3D11Texture2D,
        w: u32,
        h: u32,
        monitor: usize,
        errs: usize,
        rebuilds: usize,
    }

    pub fn opened(monitor: usize) -> Result<Box<dyn Backend>, String> {
        Dxgi::open(monitor).map(|d| Box::new(d) as Box<dyn Backend>)
    }

    struct Bound {
        dup: IDXGIOutputDuplication,
        staging: ID3D11Texture2D,
        w: u32,
        h: u32,
    }

    fn bind(monitor: usize) -> Result<(Bound, ID3D11DeviceContext), String> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1：{e}"))?;
            let adapter = factory
                .EnumAdapters1(0)
                .map_err(|e| format!("EnumAdapters1(0)：{e}（没有可用的显示适配器）"))?;
            let output = adapter
                .EnumOutputs(monitor as u32)
                .map_err(|e| format!("EnumOutputs({monitor})：{e}（没有第 {monitor} 号输出）"))?;
            let output1: IDXGIOutput1 = output.cast().map_err(|e| format!("取 IDXGIOutput1：{e}"))?;
            let mut device: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE(std::ptr::null_mut()),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .map_err(|e| format!("D3D11CreateDevice：{e}"))?;
            let device = device.ok_or("D3D11CreateDevice 没交出设备")?;
            let ctx = ctx.ok_or("D3D11CreateDevice 没交出上下文")?;
            let dup = output1
                .DuplicateOutput(&device)
                .map_err(|e| format!("DuplicateOutput：{e}（RDP / 虚机 / 基础显示驱动下通常拿不到）"))?;
            let d = dup.GetDesc();
            let (mw, mh) = (d.ModeDesc.Width, d.ModeDesc.Height);
            let staging = make_staging(&device, mw, mh)?;
            Ok((Bound { dup, staging, w: mw, h: mh }, ctx))
        }
    }

    unsafe fn make_staging(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w.max(1),
            Height: h.max(1),
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex)).map_err(|e| format!("CreateTexture2D：{e}"))?;
        tex.ok_or("CreateTexture2D 没交出纹理".to_string())
    }

    impl Dxgi {
        fn open(monitor: usize) -> Result<Self, String> {
            let (b, ctx) = bind(monitor)?;
            Ok(Dxgi { dup: b.dup, ctx, staging: b.staging, w: b.w, h: b.h, monitor, errs: 0, rebuilds: 0 })
        }

        fn rebuild(&mut self) -> Result<(), String> {
            let (b, ctx) = bind(self.monitor)?;
            self.dup = b.dup;
            self.staging = b.staging;
            self.ctx = ctx;
            self.w = b.w;
            self.h = b.h;
            self.rebuilds += 1;
            Ok(())
        }

        /// 把桌面纹理搬到 staging 再读进内存。顺序不能反：先 `Map` 再 `CopyResource`
        /// 不会报错，但读到的是上一帧或垃圾——而画面看起来完全合理。
        /// `CopyResource` 在 windows 0.62 里不返回 `Result`（它真的没什么可失败的），
        /// 所以不能 `?`；它的失败会以“下一行 `Map` 报错”的形式暴露。
        unsafe fn read_frame(
            &self,
            res: Option<IDXGIResource>,
            w: usize,
            h: usize,
        ) -> Result<Option<Frame>, String> {
            let acquired = res.ok_or("AcquireNextFrame 成功却没给资源")?;
            let desktop: ID3D11Texture2D = acquired.cast().map_err(|e| format!("取桌面纹理：{e}"))?;
            // dst = staging（CPU 可读），src = 桌面纹理（只能 GPU 访问）。反了会直接失败。
            self.ctx.CopyResource(&self.staging, &desktop);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("Map：{e}"))?;
            let pitch = mapped.RowPitch as usize;
            if mapped.pData.is_null() || pitch < w * 4 {
                self.ctx.Unmap(&self.staging, 0);
                return Err(format!("staging 行距 {pitch} 装不下 {w} 像素（或 pData 为空）"));
            }
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * h);
            let mut bgra = vec![0u8; w * 4 * h];
            for y in 0..h {
                bgra[y * w * 4..(y + 1) * w * 4].copy_from_slice(&src[y * pitch..y * pitch + w * 4]);
            }
            self.ctx.Unmap(&self.staging, 0);
            Ok(Some(Frame { width: w, height: h, rgb: bgra_to_rgb(&bgra) }))
        }
    }

    impl Backend for Dxgi {
        fn grab(&mut self) -> Result<Option<Frame>, String> {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            // 超时 0：有没有新帧由合成器说，我们只按 poll_ms 的节奏去问一次。
            unsafe {
                match self.dup.AcquireNextFrame(0, &mut info, &mut res) {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                    Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                        // 模式切换、显示器热插拔都会掐断复制接口。重建，并且把它记进数据。
                        self.errs += 1;
                        self.rebuild()?;
                        return Ok(None);
                    }
                    Err(e) => {
                        self.errs += 1;
                        return Err(format!("AcquireNextFrame：{e}"));
                    }
                }
            }
            let (w, h) = (self.w as usize, self.h as usize);
            // 不管下面读成功还是报错，纹理都必须归还：一次 ReleaseFrame 漏掉，
            // 下一整个复制接口就卡在那一帧上。
            let out = unsafe { self.read_frame(res, w, h) };
            unsafe {
                let _ = self.dup.ReleaseFrame();
            }
            if out.is_err() {
                self.errs += 1;
            }
            out
        }

        fn describe(&self) -> serde_json::Value {
            json!({
                "backend": "dxgi",
                "monitor": self.monitor,
                "width": self.w as u64,
                "height": self.h as u64,
                "rebuilds": self.rebuilds as u64,
            })
        }

        fn errors(&self) -> usize {
            self.errs
        }
    }
}

#[cfg(target_os = "linux")]
/// 把 X11 visual mask 下的颜色分量缩到 8 位。5/6 位通道直接截断会让纯白变成 248 之类：
/// 颜色错了不会报错，只会让一整节课的关键帧颜色发闷。
fn scale_component(raw: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let value = (raw & mask) >> shift;
    ((value * 255 + max / 2) / max) as u8
}

#[cfg(target_os = "linux")]
mod x11 {
    use super::{scale_component, Backend, Frame};
    use serde_json::json;
    use x11rb::connection::Connection;
    use x11rb::protocol::composite::ConnectionExt as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
    use x11rb::rust_connection::RustConnection;

    pub struct X11 {
        conn: RustConnection,
        drawable: u32,
        display: String,
        target: &'static str,
        w: u16,
        h: u16,
        depth: u8,
        bpp: u8,
        stride: usize,
        lsb: bool,
        rmask: u32,
        gmask: u32,
        bmask: u32,
        errs: usize,
        debugged: u8,
    }

    pub fn opened(monitor: usize) -> Result<super::Opened, String> {
        let (x, fallback) = X11::open(monitor)?;
        Ok(super::Opened { backend: Box::new(x) as Box<dyn Backend>, fallback })
    }

    /// 有合成器时根窗口只有背景：窗口被重定向到离屏 pixmap，由合成器画进 overlay。
    /// 用 EWMH 的选择持有者判断"有没有合成器在跑"，不要猜。
    fn compositor_running(conn: &RustConnection) -> Result<bool, String> {
        let atom = conn
            .intern_atom(false, b"_NET_WM_CM_S0")
            .map_err(|e| format!("X11 InternAtom(_NET_WM_CM_S0) 失败：{e}"))?
            .reply()
            .map_err(|e| format!("X11 InternAtom(_NET_WM_CM_S0) 回包失败：{e}"))?
            .atom;
        let owner = conn
            .get_selection_owner(atom)
            .map_err(|e| format!("X11 GetSelectionOwner 失败：{e}"))?
            .reply()
            .map_err(|e| format!("X11 GetSelectionOwner 回包失败：{e}"))?
            .owner;
        Ok(owner != 0)
    }

    fn overlay_window(conn: &RustConnection, root: u32) -> Result<Option<u32>, String> {
        conn.composite_query_version(0, 4)
            .map_err(|e| format!("X11 CompositeQueryVersion 失败：{e}"))?
            .reply()
            .map_err(|e| format!("X11 Composite 扩展不可用：{e}"))?;
        let reply = conn
            .composite_get_overlay_window(root)
            .map_err(|e| format!("X11 CompositeGetOverlayWindow 失败：{e}"))?
            .reply()
            .map_err(|e| format!("X11 CompositeGetOverlayWindow 回包失败：{e}"))?;
        Ok((reply.overlay_win != 0).then_some(reply.overlay_win))
    }

    impl X11 {
        fn open(monitor: usize) -> Result<(Self, Option<String>), String> {
            if monitor != 0 {
                return Err(format!(
                    "Linux X11 只抓整个根窗口（monitor=0），收到 {monitor}；多屏在根窗口里是拼接画面"
                ));
            }
            // Wayland 下 XWayland 的根窗口不是整块桌面，抓到的多半是黑屏或局部。
            // 宁可让这个源从健康表缺席，也不把"没采到"伪装成一节黑屏课。
            let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var("XDG_SESSION_TYPE")
                    .map(|v| v.eq_ignore_ascii_case("wayland"))
                    .unwrap_or(false);
            if wayland {
                return Err(
                    "当前是 Wayland 会话：X11 只能抓到 XWayland 的根窗口，不是整块桌面。请切到 X11 会话，或等待 PipeWire 后端。"
                        .into(),
                );
            }
            let display = std::env::var("DISPLAY").unwrap_or_default();
            let (conn, screen_num) = x11rb::connect(None)
                .map_err(|e| format!("连不上 X11 display（DISPLAY={display:?}）：{e}"))?;
            let (root, w, h, depth, bpp, stride, lsb, order, (rmask, gmask, bmask)) = {
                let setup = conn.setup();
                let screen = &setup.roots[screen_num];
                if screen.width_in_pixels == 0 || screen.height_in_pixels == 0 {
                    return Err(format!(
                        "X11 根窗口尺寸是 {}x{}，没有可采的桌面",
                        screen.width_in_pixels, screen.height_in_pixels
                    ));
                }
                let visual = screen
                    .allowed_depths
                    .iter()
                    .flat_map(|d| d.visuals.iter())
                    .find(|v| v.visual_id == screen.root_visual)
                    .ok_or_else(|| {
                        format!("X11 根窗口的 visual {} 不在 allowed_depths 里", screen.root_visual)
                    })?;
                let fmt = setup
                    .pixmap_formats
                    .iter()
                    .find(|f| f.depth == screen.root_depth)
                    .ok_or_else(|| format!("X11 没有 depth {} 的 pixmap 格式", screen.root_depth))?;
                if !matches!(fmt.bits_per_pixel, 16 | 24 | 32) {
                    return Err(format!(
                        "X11 像素格式 {} bpp 暂不支持（只支持 16 / 24 / 32）",
                        fmt.bits_per_pixel
                    ));
                }
                if visual.red_mask == 0 && visual.green_mask == 0 && visual.blue_mask == 0 {
                    return Err("X11 根窗口的 visual 没有 RGB mask（伪彩？），无法还原颜色".into());
                }
                let pad = fmt.scanline_pad as usize;
                if pad == 0 {
                    return Err("X11 pixmap 格式的 scanline_pad 是 0，无法算行距".into());
                }
                let stride = (screen.width_in_pixels as usize * fmt.bits_per_pixel as usize)
                    .div_ceil(pad)
                    * pad
                    / 8;
                (
                    screen.root,
                    screen.width_in_pixels,
                    screen.height_in_pixels,
                    screen.root_depth,
                    fmt.bits_per_pixel,
                    stride,
                    u8::from(setup.image_byte_order) == 0,
                    u8::from(setup.image_byte_order),
                    (visual.red_mask, visual.green_mask, visual.blue_mask),
                )
            };
            let (drawable, target, fallback) = if compositor_running(&conn)? {
                match overlay_window(&conn, root) {
                    Ok(Some(win)) => (win, "composite-overlay", None),
                    Ok(None) => (
                        root,
                        "root",
                        Some("合成器在跑但 Composite overlay 是 0，退回根窗口".to_string()),
                    ),
                    Err(e) => (
                        root,
                        "root",
                        Some(format!("合成器在跑但拿不到 Composite overlay，退回根窗口：{e}")),
                    ),
                }
            } else {
                (root, "root", None)
            };
            eprintln!(
                "[a-screen] X11 就绪：display={display} {target} {w}x{h}，depth={depth}，{bpp}bpp，{} 字节/行，image_order={order}，masks={rmask:06x}/{gmask:06x}/{bmask:06x}",
                stride
            );
            Ok((
                X11 {
                    conn,
                    drawable,
                    display,
                    target,
                    w,
                    h,
                    depth,
                    bpp,
                    stride,
                    lsb,
                    rmask,
                    gmask,
                    bmask,
                    errs: 0,
                    debugged: 0,
                },
                fallback,
            ))
        }
    }

    impl Backend for X11 {
        fn grab(&mut self) -> Result<Option<Frame>, String> {
            let reply = match self
                .conn
                .get_image(ImageFormat::Z_PIXMAP, self.drawable, 0, 0, self.w, self.h, !0)
                .map_err(|e| format!("X11 GetImage 请求失败：{e}"))
                .and_then(|c| c.reply().map_err(|e| format!("X11 GetImage 回包失败：{e}")))
            {
                Ok(r) => r,
                Err(e) => {
                    self.errs += 1;
                    return Err(e);
                }
            };
            if reply.depth != self.depth {
                self.errs += 1;
                return Err(format!(
                    "X11 GetImage 回来的 depth 是 {}，预期 {}（overlay 与根窗口格式不一致）",
                    reply.depth, self.depth
                ));
            }
            self.debugged = self.debugged.wrapping_add(1);
            if self.debugged == 1 || self.debugged % 20 == 0 {
                let n = reply.data.len().min(8);
                eprintln!("[a-screen] X11 第 {} 次询问前 {n} 字节：{:02x?}", self.debugged, &reply.data[..n]);
            }
            let (w, h, bpp) = (self.w as usize, self.h as usize, self.bpp as usize);
            let bytes_per_pixel = bpp / 8;
            let need = self.stride * h;
            if reply.data.len() < need || bytes_per_pixel == 0 {
                self.errs += 1;
                return Err(format!(
                    "X11 GetImage 回了 {} 字节，{w}x{h}@{bpp}bpp 至少要 {need}",
                    reply.data.len()
                ));
            }
            let mut rgb = Vec::with_capacity(w * h * 3);
            for y in 0..h {
                let row = &reply.data[y * self.stride..y * self.stride + w * bytes_per_pixel];
                for x in 0..w {
                    let p = &row[x * bytes_per_pixel..(x + 1) * bytes_per_pixel];
                    let raw = if self.lsb {
                        p.iter().enumerate().fold(0u32, |acc, (i, b)| acc | ((*b as u32) << (8 * i)))
                    } else {
                        p.iter().fold(0u32, |acc, b| (acc << 8) | *b as u32)
                    };
                    rgb.push(scale_component(raw, self.rmask));
                    rgb.push(scale_component(raw, self.gmask));
                    rgb.push(scale_component(raw, self.bmask));
                }
            }
            Ok(Some(Frame { width: w, height: h, rgb }))
        }

        fn describe(&self) -> serde_json::Value {
            json!({
                "backend": "x11",
                "monitor": 0,
                "display": self.display,
                "target": self.target,
                "width": self.w as u64,
                "height": self.h as u64,
                "depth": self.depth,
                "bits_per_pixel": self.bpp,
                "note": "X11 有合成器时抓 composite overlay，否则抓根窗口；多屏为拼接画面，Wayland / PipeWire 未实现",
            })
        }

        fn errors(&self) -> usize {
            self.errs
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_list_is_the_contract_the_ui_copies() {
        assert_eq!(BACKENDS, ["auto", "gdi", "dxgi", "x11"]);
    }

    #[test]
    fn an_unknown_capture_is_refused_not_silently_switched() {
        // 不用 unwrap_err：它要求 Ok 那一侧（Opened）也实现 Debug，而 Box<dyn Backend> 不满足。
        let e = open("quartz", 0).err().expect("清单外的后端必须被当场拒，不能悄悄换一条路");
        #[cfg(any(windows, target_os = "linux"))]
        assert!(e.contains("capture"), "{e}");
        #[cfg(not(any(windows, target_os = "linux")))]
        assert!(e.contains("可用的设备抓屏"), "{e}");
    }

    #[cfg(windows)]
    #[test]
    fn linux_backend_is_refused_on_windows() {
        let e = open("x11", 0).err().expect("x11 不该在 Windows 上被接受");
        assert!(e.contains("Linux"), "{e}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn windows_only_backends_are_refused_on_linux() {
        let e = open("gdi", 0).err().expect("gdi 不该在 Linux 上被接受");
        assert!(e.contains("Windows"), "{e}");
        let e = open("dxgi", 0).err().expect("dxgi 不该在 Linux 上被接受");
        assert!(e.contains("Windows"), "{e}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn x11_components_are_scaled_not_truncated() {
        // 8 位通道原样；5 / 6 位通道的最大值必须回到 255，不能停在 248 / 252。
        assert_eq!(scale_component(0x00ff0000, 0x00ff0000), 255);
        assert_eq!(scale_component(0x0000f800, 0x0000f800), 255);
        assert_eq!(scale_component(0x0000003f, 0x0000003f), 255);
        assert_eq!(scale_component(0, 0x00ff0000), 0);
        // 缺通道（伪彩 / 灰度 visual）不 panic，按 0 处理。
        assert_eq!(scale_component(0xffffffff, 0), 0);
    }

    #[test]
    fn bgra_is_unrolled_to_rgb() {
        assert_eq!(bgra_to_rgb(&[10, 20, 30, 255, 40, 50, 60, 0]), vec![30, 20, 10, 60, 50, 40]);
        // 六字节 = 一个整像素 + 两个尾巴：整像素照旧产出，尾巴被丢掉而不 panic。
        // is_empty() 等于要求“不足 4 字节就全丢”，但实现不是这么写的。
        assert_eq!(bgra_to_rgb(&[0, 0, 0, 0, 0, 0]), vec![0, 0, 0], "不足一像素的尾巴被丢掉，不 panic");
        assert!(bgra_to_rgb(&[0, 0, 0]).is_empty(), "不足一像素就什么都没有");
    }
}
