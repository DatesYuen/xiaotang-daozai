//! 【新增】协议注册与纯解析。按接口能力匹配，不对未知品牌套用相似报文。
use super::{Battery, Descriptor, Failure};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Protocol {
    Standard,
    Compx { online: bool },
    Hidpp,
    Rapoo,
    Bytech,
}
impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "standard-hid",
            Self::Compx { .. } => "compx-17",
            Self::Hidpp => "hidpp-2",
            Self::Rapoo => "rapoo-x2",
            Self::Bytech => "bytech-65",
        }
    }
}

/// 【新增】新增适配器只改注册与协议实现；标准电池字段始终优先，允许厂商协议补充失败路径。
pub(super) fn candidates(d: &Descriptor) -> Vec<Protocol> {
    let mut result = Vec::new();
    if d.fields.iter().any(|f| f.battery_usage().is_some()) {
        result.push(Protocol::Standard);
    }
    let compx = matches!(
        (d.vid, d.pid),
        (0x3554, 0xfb3e | 0xfb3d | 0xfb34) | (0x373b, 0x1278)
    );
    if compx && d.usage_page == 0xff02 && d.usage == 2 && d.input_len == 17 && d.output_len == 17 {
        result.push(Protocol::Compx {
            online: d.vid == 0x3554 && d.pid == 0xfb3e,
        });
    }
    if d.vid == 0x046d
        && d.usage_page == 0xff00
        && d.usage == 2
        && d.output_len == 20
        && d.input_len >= 20
    {
        result.push(Protocol::Hidpp);
    }
    // 【新增】雷柏 VT3PRO 等接收器：命令集合 col04(FF00/000E) 只写，回复从 col05(FF00/0002) 读。
    if (d.vid, d.pid) == (0x24ae, 0x1215)
        && d.usage_page == 0xff00
        && d.usage == 0x000e
        && d.output_len == 32
    {
        result.push(Protocol::Rapoo);
    }
    // 【新增】狼蛛等 bytech 固件键盘：单集合 FF60/0061 同句柄写读，65 字节帧。
    if (d.vid, d.pid) == (0x0c45, 0xfef9)
        && d.usage_page == 0xff60
        && d.usage == 0x0061
        && d.output_len == 65
    {
        result.push(Protocol::Bytech);
    }
    result
}

/// 平台 I/O 的最小边界，也供假设备测试使用；调用方负责整轮截止时间及取消。
pub(super) trait Io {
    fn write(&mut self, request: &[u8], control: bool) -> Result<(), Failure>;
    fn read(&mut self, timeout: Duration) -> Result<Vec<u8>, Failure>;
    fn check(&self) -> Result<(), Failure>;
    /// 【新增】跨集合协议（雷柏）的第二读通道；回复只投递到另一个集合的输入管道。
    /// 平台层需在首次写入前打开它，避免错过回复帧。默认不支持。
    fn read_companion(&mut self, _timeout: Duration) -> Result<Vec<u8>, Failure> {
        Err(Failure::Unsupported)
    }
}

fn validate_compx(frame: &[u8], command: u8) -> Result<(), Failure> {
    if frame.len() != 17
        || frame[..2] != [8, command]
        || frame[2] != 0
        || frame[5] == 0
        || frame[1..].iter().fold(0u8, |sum, b| sum.wrapping_add(*b)) != 0x4d
    {
        return Err(Failure::InvalidReply);
    }
    Ok(())
}
pub(super) fn parse_compx(frame: &[u8]) -> Result<Battery, Failure> {
    validate_compx(frame, 4)?;
    if frame[5] != 2 || frame[6] > 100 || frame[7] > 1 {
        return Err(Failure::InvalidReply);
    }
    Ok(Battery {
        percent: frame[6],
        charging: Some(frame[7] == 1),
        millivolts: Some(u16::from_be_bytes([frame[8], frame[9]])),
    })
}
fn compx_exchange(io: &mut impl Io, command: u8) -> Result<Vec<u8>, Failure> {
    let mut request = [0u8; 17];
    request[0] = 8;
    request[1] = command;
    request[16] = 0x4du8.wrapping_sub(command);
    let mut last = Failure::Timeout;
    // 【新增】两种已知发送方式各一次；读取切成短片，关闭功能时能及时取消。
    for control in [false, true] {
        io.check()?;
        if let Err(error) = io.write(&request, control) {
            last = error;
            continue;
        }
        let until = Instant::now() + Duration::from_millis(700);
        while Instant::now() < until {
            io.check()?;
            let frame = io.read(Duration::from_millis(60))?;
            if frame.is_empty() {
                continue;
            }
            if frame.starts_with(&[8, command]) {
                match validate_compx(&frame, command) {
                    Ok(()) => return Ok(frame),
                    Err(e) => last = e,
                }
            }
        }
    }
    Err(last)
}
pub(super) fn read_compx(io: &mut impl Io, online: bool) -> Result<Battery, Failure> {
    if online {
        let reply = compx_exchange(io, 3)?;
        if reply[5] != 1 {
            return Err(Failure::InvalidReply);
        }
        if reply[6] == 0 {
            return Err(Failure::Offline);
        }
    }
    parse_compx(&compx_exchange(io, 4)?)
}

/// 【新增】雷柏 2.4G 接收器（24AE:1215 实测 39/40）：写 col04 报告 ID 0xBA 触发查询，
/// 从 col05 读 7 字节回复 `BB B0 51 40 06 01 <pct>`，电量在字节 6；不报告充电状态。
fn parse_rapoo(frame: &[u8]) -> Result<Battery, Failure> {
    if frame.len() < 7 || frame[0] != 0xBB || frame[6] > 100 {
        return Err(Failure::InvalidReply);
    }
    Ok(Battery {
        percent: frame[6],
        charging: None,
        millivolts: None,
    })
}
pub(super) fn read_rapoo(io: &mut impl Io) -> Result<Battery, Failure> {
    let mut request = [0u8; 32];
    request[0] = 0xBA;
    let mut last = Failure::Timeout;
    // 接收器偶发不回包（实测约 1/40），重试 3 次；读取切成短片保证能及时取消。
    for _ in 0..3 {
        io.check()?;
        if let Err(error) = io.write(&request, false) {
            last = error;
            continue;
        }
        let until = Instant::now() + Duration::from_millis(700);
        while Instant::now() < until {
            io.check()?;
            let frame = match io.read_companion(Duration::from_millis(80)) {
                Ok(frame) => frame,
                // 该轮确实无回复（平台层超时），进入下一次重试。
                Err(Failure::Timeout) => break,
                Err(error) => return Err(error),
            };
            if frame.is_empty() {
                continue;
            }
            if frame[0] == 0xBB {
                return parse_rapoo(&frame);
            }
            last = Failure::InvalidReply;
        }
    }
    Err(last)
}

/// 【新增】狼蛛等 bytech 固件（0C45:FEF9 实测 8/8）：同一集合写读 65 字节帧，
/// 请求 `00 AA 10 38 00 00 00 01 00 + 56×00`，回复 `00 55 10 38 ...`，
/// 电量在字节 26、充电态在字节 27（>0 表示充电中）。
fn parse_bytech(frame: &[u8]) -> Result<Battery, Failure> {
    if frame.len() <= 27 || frame[..4] != [0x00, 0x55, 0x10, 0x38] || frame[26] > 100 {
        return Err(Failure::InvalidReply);
    }
    Ok(Battery {
        percent: frame[26],
        charging: Some(frame[27] > 0),
        millivolts: None,
    })
}
pub(super) fn read_bytech(io: &mut impl Io) -> Result<Battery, Failure> {
    // 首字节 0 是 Windows 无报告 ID 的写法；该集合只回显对查询的回复，不推送按键。
    let mut request = [0u8; 65];
    request[1..9].copy_from_slice(&[0xAA, 0x10, 0x38, 0x00, 0x00, 0x00, 0x01, 0x00]);
    let mut last = Failure::Timeout;
    for _ in 0..2 {
        io.check()?;
        if let Err(error) = io.write(&request, false) {
            last = error;
            continue;
        }
        let until = Instant::now() + Duration::from_millis(700);
        while Instant::now() < until {
            io.check()?;
            let frame = match io.read(Duration::from_millis(80)) {
                Ok(frame) => frame,
                // 该轮确实无回复（平台层超时），进入下一次重试。
                Err(Failure::Timeout) => break,
                Err(error) => return Err(error),
            };
            if frame.is_empty() || frame[0] != 0x00 {
                continue;
            }
            if frame.len() > 3 && frame[1] == 0x55 && frame[2] == 0x10 && frame[3] == 0x38 {
                return parse_bytech(&frame);
            }
            last = Failure::InvalidReply;
        }
    }
    Err(last)
}

/// 【新增】HID++ 只查询 feature 与电量，不修改配对、通知、DPI 或灯光。
pub(super) struct Hidpp {
    software_id: u8,
}
impl Hidpp {
    pub fn new() -> Self {
        Self { software_id: 8 }
    }
    fn call(
        &mut self,
        io: &mut impl Io,
        slot: u8,
        feature: u8,
        function: u8,
        parameters: &[u8],
    ) -> Result<Vec<u8>, Failure> {
        self.software_id = self.software_id % 15 + 1;
        let mut request = [0u8; 20];
        request[..4].copy_from_slice(&[0x11, slot, feature, (function << 4) | self.software_id]);
        request[4..4 + parameters.len()].copy_from_slice(parameters);
        io.check()?;
        io.write(&request, false)?;
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            io.check()?;
            let frame = io.read(Duration::from_millis(50))?;
            if hidpp_matches(&frame, &request) {
                return Ok(frame);
            }
            // 错误回复也必须匹配原 feature / function / 软件号，不能吞掉其他程序的错误。
            if frame.len() >= 7
                && matches!(frame[0], 0x10 | 0x11)
                && frame[1] == slot
                && frame[2] == 0xff
                && frame[3] == feature
                && frame[4] == request[3]
            {
                return Err(Failure::Unsupported);
            }
        }
        Err(Failure::Timeout)
    }
    pub fn read(&mut self, io: &mut impl Io, slot: u8) -> Result<Battery, Failure> {
        // 1000 = Battery Level Status；1004 = Unified Battery，不能将后者误作电压协议。
        for feature in [0x1000u16, 0x1004] {
            let id = feature.to_be_bytes();
            let root = self.call(io, slot, 0, 0, &id)?;
            let index = root[4];
            if index == 0 {
                continue;
            }
            if feature == 0x1004 {
                let capabilities = self.call(io, slot, index, 0, &[])?;
                // 没有百分比能力时不把等级、0 或电压冒充精确百分比。
                if capabilities[5] & 2 == 0 {
                    continue;
                }
            }
            let response =
                self.call(io, slot, index, if feature == 0x1004 { 1 } else { 0 }, &[])?;
            return parse_hidpp(feature, &response);
        }
        Err(Failure::Unsupported)
    }
}
fn hidpp_matches(reply: &[u8], request: &[u8]) -> bool {
    matches!(reply.len(), 7 | 20) && matches!(reply[0], 0x10 | 0x11) && reply[1..4] == request[1..4]
}
fn parse_hidpp(feature: u16, frame: &[u8]) -> Result<Battery, Failure> {
    if frame.len() < 7 || frame[4] > 100 {
        return Err(Failure::InvalidReply);
    }
    let status = frame[6];
    if status > 4 {
        return Err(Failure::InvalidReply);
    }
    let charging = match feature {
        0x1000 | 0x1004 => Some(matches!(status, 1 | 2)),
        _ => return Err(Failure::Unsupported),
    };
    Ok(Battery {
        percent: frame[4],
        charging,
        millivolts: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_uses_protocol_and_collection_not_brand_name() {
        let mut d = Descriptor {
            vid: 0x3554,
            pid: 0xfb34,
            usage_page: 0xff02,
            usage: 2,
            input_len: 17,
            output_len: 17,
            ..Default::default()
        };
        assert_eq!(candidates(&d), vec![Protocol::Compx { online: false }]);
        d.vid = 0x373b;
        d.pid = 0x1278;
        assert_eq!(candidates(&d).len(), 1);
        d.pid = 0x1262;
        assert!(candidates(&d).is_empty());
        d.vid = 0x3554;
        d.pid = 0xfb3e;
        d.usage_page = 0xff04;
        assert!(candidates(&d).is_empty());
    }
    #[test]
    fn rapoo_and_bytech_register_only_on_measured_collections() {
        // 雷柏：命令集合 col04(FF00/000E, 输出 32B)；其他集合/长度/型号都不注册。
        let mut d = Descriptor {
            vid: 0x24ae,
            pid: 0x1215,
            usage_page: 0xff00,
            usage: 0x000e,
            output_len: 32,
            ..Default::default()
        };
        assert_eq!(candidates(&d), vec![Protocol::Rapoo]);
        d.usage = 0x0002;
        assert!(candidates(&d).is_empty());
        d.usage = 0x000e;
        d.output_len = 33;
        assert!(candidates(&d).is_empty());
        d.output_len = 32;
        d.pid = 0x1216;
        assert!(candidates(&d).is_empty());
        // 狼蛛：bytech 单集合 FF60/0061，输出 65B。
        let mut k = Descriptor {
            vid: 0x0c45,
            pid: 0xfef9,
            usage_page: 0xff60,
            usage: 0x0061,
            output_len: 65,
            ..Default::default()
        };
        assert_eq!(candidates(&k), vec![Protocol::Bytech]);
        k.usage = 0x0071;
        assert!(candidates(&k).is_empty());
        k.usage = 0x0061;
        k.pid = 0xfefa;
        assert!(candidates(&k).is_empty());
    }
    #[test]
    fn rapoo_reply_percent_sits_in_byte_six() {
        // 实测回复帧：BB B0 51 40 06 01 2C -> 44%。
        let frame = [0xBB, 0xB0, 0x51, 0x40, 0x06, 0x01, 44];
        assert_eq!(parse_rapoo(&frame).unwrap().percent, 44);
        assert_eq!(parse_rapoo(&frame).unwrap().charging, None);
        let mut full = frame;
        full[6] = 100;
        assert_eq!(parse_rapoo(&full).unwrap().percent, 100);
        assert_eq!(parse_rapoo(&[0xBB, 0, 0, 0, 0, 0, 101]), Err(Failure::InvalidReply));
        assert_eq!(parse_rapoo(&[0xBB, 0, 0, 0, 0, 0]), Err(Failure::InvalidReply));
        assert_eq!(parse_rapoo(&[0xBA, 0, 0, 0, 0, 0, 44]), Err(Failure::InvalidReply));
    }
    #[test]
    fn bytech_reply_reports_percent_and_charging() {
        // 实测回复帧：00 55 10 38 ...，电量在字节 26（0x64=100%），充电态在字节 27。
        let mut frame = [0u8; 65];
        frame[1] = 0x55;
        frame[2] = 0x10;
        frame[3] = 0x38;
        frame[26] = 100;
        assert_eq!(parse_bytech(&frame).unwrap().percent, 100);
        assert_eq!(parse_bytech(&frame).unwrap().charging, Some(false));
        frame[26] = 44;
        frame[27] = 1;
        let battery = parse_bytech(&frame).unwrap();
        assert_eq!(battery.percent, 44);
        assert_eq!(battery.charging, Some(true));
        frame[1] = 0x54;
        assert_eq!(parse_bytech(&frame), Err(Failure::InvalidReply));
        frame[1] = 0x55;
        frame[26] = 101;
        assert_eq!(parse_bytech(&frame), Err(Failure::InvalidReply));
        assert_eq!(parse_bytech(&[0u8; 27]), Err(Failure::InvalidReply));
    }
    #[test]
    fn rapoo_reads_companion_channel_and_retries_on_silence() {
        struct CompanionIo {
            replies: std::collections::VecDeque<Vec<u8>>,
            writes: Vec<Vec<u8>>,
            cancelled: bool,
        }
        impl Io for CompanionIo {
            fn write(&mut self, request: &[u8], _: bool) -> Result<(), Failure> {
                self.writes.push(request.to_vec());
                Ok(())
            }
            fn read(&mut self, _: Duration) -> Result<Vec<u8>, Failure> {
                Ok(Vec::new())
            }
            fn check(&self) -> Result<(), Failure> {
                if self.cancelled {
                    Err(Failure::Cancelled)
                } else {
                    Ok(())
                }
            }
            fn read_companion(&mut self, _: Duration) -> Result<Vec<u8>, Failure> {
                self.replies.pop_front().ok_or(Failure::Timeout)
            }
        }
        // 回复出现在伴随通道而不是写通道；写 32 字节、报告 ID 0xBA。
        let mut io = CompanionIo {
            replies: vec![vec![0xBB, 0xB0, 0x51, 0x40, 0x06, 0x01, 44]].into(),
            writes: vec![],
            cancelled: false,
        };
        assert_eq!(read_rapoo(&mut io).unwrap().percent, 44);
        assert_eq!(io.writes.len(), 1);
        assert_eq!(io.writes[0].len(), 32);
        assert_eq!(io.writes[0][0], 0xBA);
        assert!(io.writes[0][1..].iter().all(|b| *b == 0));
        // 无回复时重试后放弃，取消能立即中断。
        let mut silent = CompanionIo {
            replies: vec![].into(),
            writes: vec![],
            cancelled: false,
        };
        assert_eq!(read_rapoo(&mut silent), Err(Failure::Timeout));
        assert_eq!(silent.writes.len(), 3);
        silent.cancelled = true;
        silent.replies.push_back(vec![0xBB]);
        assert_eq!(read_rapoo(&mut silent), Err(Failure::Cancelled));
    }
    #[test]
    fn bytech_writes_same_handle_and_parses_reply() {
        struct EchoIo {
            replies: std::collections::VecDeque<Vec<u8>>,
            writes: Vec<Vec<u8>>,
        }
        impl Io for EchoIo {
            fn write(&mut self, request: &[u8], _: bool) -> Result<(), Failure> {
                self.writes.push(request.to_vec());
                Ok(())
            }
            fn read(&mut self, _: Duration) -> Result<Vec<u8>, Failure> {
                self.replies.pop_front().ok_or(Failure::Timeout)
            }
            fn check(&self) -> Result<(), Failure> {
                Ok(())
            }
        }
        let mut frame = [0u8; 65];
        frame[1] = 0x55;
        frame[2] = 0x10;
        frame[3] = 0x38;
        frame[26] = 87;
        frame[27] = 0;
        let mut io = EchoIo {
            replies: vec![frame.to_vec()].into(),
            writes: vec![],
        };
        assert_eq!(read_bytech(&mut io).unwrap().percent, 87);
        assert_eq!(io.writes.len(), 1);
        assert_eq!(io.writes[0].len(), 65);
        assert_eq!(io.writes[0][0], 0x00);
        assert_eq!(&io.writes[0][1..9], &[0xAA, 0x10, 0x38, 0, 0, 0, 1, 0]);
        assert!(io.writes[0][9..].iter().all(|b| *b == 0));
        // 无回复重试两次后放弃。
        let mut silent = EchoIo {
            replies: vec![].into(),
            writes: vec![],
        };
        assert_eq!(read_bytech(&mut silent), Err(Failure::Timeout));
        assert_eq!(silent.writes.len(), 2);
    }
    #[test]
    fn compx_captures_and_corruption() {
        for (mut frame, percent) in [
            ([8, 4, 0, 0, 0, 2, 20, 0, 14, 12, 0, 0, 0, 0, 0, 0, 25], 20),
            ([8, 4, 0, 0, 0, 2, 30, 0, 14, 90, 0, 0, 0, 0, 0, 0, 193], 30),
            (
                [8, 4, 0, 0, 0, 2, 100, 0, 16, 221, 0, 0, 0, 0, 0, 0, 246],
                100,
            ),
        ] {
            assert_eq!(parse_compx(&frame).unwrap().percent, percent);
            frame[6] ^= 1;
            assert_eq!(parse_compx(&frame), Err(Failure::InvalidReply));
        }
        assert_eq!(parse_compx(&[0; 16]), Err(Failure::InvalidReply));
    }
    #[test]
    fn hidpp_rejects_other_slots_software_ids_and_invalid_values() {
        let request = [0x11, 2, 6, 0x1e, 0, 0, 0];
        let mut reply = request;
        assert!(hidpp_matches(&reply, &request));
        reply[1] = 1;
        assert!(!hidpp_matches(&reply, &request));
        reply = request;
        reply[3] = 0x1d;
        assert!(!hidpp_matches(&reply, &request));
        assert_eq!(
            parse_hidpp(0x1004, &[0x11, 2, 6, 0x1e, 97, 8, 1])
                .unwrap()
                .percent,
            97
        );
        assert_eq!(
            parse_hidpp(0x1000, &[0x11, 2, 6, 0x1e, 101, 0, 0]),
            Err(Failure::InvalidReply)
        );
    }
    struct FakeIo {
        replies: std::collections::VecDeque<Vec<u8>>,
        writes: Vec<Vec<u8>>,
        cancelled: bool,
    }
    impl Io for FakeIo {
        fn check(&self) -> Result<(), Failure> {
            if self.cancelled {
                Err(Failure::Cancelled)
            } else {
                Ok(())
            }
        }
        fn write(&mut self, request: &[u8], _: bool) -> Result<(), Failure> {
            self.writes.push(request.to_vec());
            Ok(())
        }
        fn read(&mut self, _: Duration) -> Result<Vec<u8>, Failure> {
            self.replies.pop_front().ok_or(Failure::Timeout)
        }
    }
    #[test]
    fn compx_queries_online_before_battery_and_stops_when_offline() {
        let mut io = FakeIo {
            replies: vec![vec![8, 3, 0, 0, 0, 1, 0, 89, 30, 135, 0, 0, 0, 0, 0, 0, 75]].into(),
            writes: vec![],
            cancelled: false,
        };
        assert_eq!(read_compx(&mut io, true), Err(Failure::Offline));
        assert_eq!(io.writes.len(), 1);
        assert_eq!(io.writes[0][1], 3);
        io.cancelled = true;
        assert_eq!(read_compx(&mut io, false), Err(Failure::Cancelled));
        assert_eq!(io.writes.len(), 1);
    }
    #[test]
    fn hidpp_discovers_features_and_ignores_another_apps_reply() {
        let mut io = FakeIo {
            replies: vec![
                vec![0x11, 2, 0, 8, 5, 0, 0],
                vec![0x11, 2, 0, 9, 5, 0, 0],
                vec![0x11, 2, 5, 10, 75, 0, 1],
            ]
            .into(),
            writes: vec![],
            cancelled: false,
        };
        let battery = Hidpp::new().read(&mut io, 2).unwrap();
        assert_eq!(battery.percent, 75);
        assert_eq!(battery.charging, Some(true));
        assert_eq!(&io.writes[0][4..6], &[0x10, 0]);
        assert_eq!(io.writes[1][2], 5);
    }
    #[test]
    fn unified_battery_checks_percentage_capability_before_status() {
        let mut io = FakeIo {
            replies: vec![
                vec![0x11, 1, 0, 9, 0, 0, 0],
                vec![0x11, 1, 0, 10, 6, 0, 0],
                vec![0x11, 1, 6, 11, 15, 2, 0],
                vec![0x11, 1, 6, 28, 97, 8, 0],
            ]
            .into(),
            writes: vec![],
            cancelled: false,
        };
        assert_eq!(Hidpp::new().read(&mut io, 1).unwrap().percent, 97);
        assert_eq!(io.writes[3][3], 28);
        let mut io = FakeIo {
            replies: vec![
                vec![0x11, 1, 0, 9, 0, 0, 0],
                vec![0x11, 1, 0, 10, 6, 0, 0],
                vec![0x11, 1, 6, 11, 15, 0, 0],
            ]
            .into(),
            writes: vec![],
            cancelled: false,
        };
        assert_eq!(Hidpp::new().read(&mut io, 1), Err(Failure::Unsupported));
        assert_eq!(io.writes.len(), 3);
    }
    #[test]
    fn compx_rejects_echo_bad_flags_and_accepts_zero_and_full() {
        let mut frame = [8, 4, 0, 0, 0, 2, 0, 0, 14, 12, 0, 0, 0, 0, 0, 0, 0];
        for percent in [0, 100, 101] {
            frame[6] = percent;
            frame[16] =
                0x4du8.wrapping_sub(frame[1..16].iter().fold(0u8, |sum, b| sum.wrapping_add(*b)));
            assert_eq!(parse_compx(&frame).is_ok(), percent <= 100);
        }
        frame[6] = 80;
        frame[7] = 2;
        frame[16] =
            0x4du8.wrapping_sub(frame[1..16].iter().fold(0u8, |sum, b| sum.wrapping_add(*b)));
        assert_eq!(parse_compx(&frame), Err(Failure::InvalidReply));
        frame[5] = 0;
        assert_eq!(parse_compx(&frame), Err(Failure::InvalidReply));
    }
}
