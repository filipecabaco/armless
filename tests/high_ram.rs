//! RAM above 4 GiB: a machine of more than 3 GiB has the rest of its RAM from 4 GiB on (past the
//! MMIO hole), and a bare-metal guest writes, reads and load-exclusives it there.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use armless::dev::Event;
use armless::native::{HostOptions, NativeHost};
use armless::runner::{self, Runner, Stopped};
use armless::vm::Config;

/// The guest, assembled (clang --target=aarch64-none-elf) from:
///
/// ```text
///     movz x9, #0xf010, lsl #16       // UART
///     movz x0, #1, lsl #32            // 4 GiB
///     mov  x3, #0
/// 1:  lsl  x5, x3, #12                // a word in each of 1024 pages: i + 0x55
///     add  x7, x3, #0x55
///     str  x7, [x0, x5]
///     add  x3, x3, #1
///     cmp  x3, #1024
///     b.ne 1b
///     mov  x3, #0
///     mov  x8, #0
/// 2:  lsl  x5, x3, #12                // read back and summed
///     ldr  x7, [x0, x5]
///     add  x8, x8, x7
///     add  x3, x3, #1
///     cmp  x3, #1024
///     b.ne 2b
///     movz x10, #0x5200
///     movk x10, #0x9, lsl #16         // sum of i + 0x55 for i < 1024
///     cmp  x8, x10
///     b.ne 4f
/// 3:  ldxr x11, [x0]                  // an exclusive pair on the first
///     add  x11, x11, #1
///     stxr w12, x11, [x0]
///     cbnz w12, 3b
///     ldr  x11, [x0]
///     cmp  x11, #0x56
///     b.ne 4f
///     mov  w1, #'O'
///     strb w1, [x9]
///     mov  w1, #'K'
///     b    5f
/// 4:  mov  w1, #'N'
///     strb w1, [x9]
///     mov  w1, #'O'
/// 5:  strb w1, [x9]
///     mov  w1, #'\n'
///     strb w1, [x9]
///     movz x0, #0x0008                // PSCI SYSTEM_OFF
///     movk x0, #0x8400, lsl #16
///     hvc  #0
/// 6:  wfi
///     b    6b
/// ```
///
/// tests/js/high-ram.test.mjs runs the same words on the browser build.
const GUEST: [u32; 43] = [
    0xd2be0209, 0xd2c00020, 0xd2800003, 0xd374cc65, 0x91015467, 0xf8256807, 0x91000463, 0xf110007f,
    0x54ffff61, 0xd2800003, 0xd2800008, 0xd374cc65, 0xf8656807, 0x8b070108, 0x91000463, 0xf110007f,
    0x54ffff61, 0xd28a400a, 0xf2a0012a, 0xeb0a011f, 0x54000181, 0xc85f7c0b, 0x9100056b, 0xc80c7c0b,
    0x35ffffac, 0xf940000b, 0xf101597f, 0x540000a1, 0x528009e1, 0x39000121, 0x52800961, 0x14000004,
    0x528009c1, 0x39000121, 0x528009e1, 0x39000121, 0x52800141, 0x39000121, 0xd2800100, 0xf2b08000,
    0xd4000002, 0xd503207f, 0x17ffffff,
];

fn run(cpus: usize) {
    let image: Vec<u8> = GUEST.iter().flat_map(|w| w.to_le_bytes()).collect();
    let (mut vm, clock) = runner::new_vm(Config {
        memory_size: armless::board::LOW_RAM_MAX + (8 << 20),
        cpus,
        ..Default::default()
    })
    .unwrap();
    armless::boot_raw(&mut vm.cpus[0], &image, armless::board::KERNEL_OFFSET).unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let o = output.clone();
    let _host = NativeHost::spawn(
        vm.board.clone(),
        HostOptions::default(),
        Box::new(move |e| {
            if let Event::Serial(b) = e {
                o.lock().unwrap().extend_from_slice(b);
            }
        }),
    );
    let mut runner = Runner::new(vm, clock, true);
    runner.start();
    let exit = runner.wait(Some(Duration::from_secs(60)));
    std::thread::sleep(Duration::from_millis(200));
    let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
    assert_eq!(exit, Some(Stopped::PowerOff), "the guest did not finish:\n{text}");
    assert_eq!(text, "OK\n");
}

#[test]
fn a_guest_uses_ram_above_4_gib() {
    run(1);
}

#[test]
fn a_guest_uses_ram_above_4_gib_with_two_cpus() {
    // Exclusives as compare-and-swap, as with any several CPUs
    run(2);
}
