// The C shell of every RP2 firmware.
//
// This file is deliberately everything pico-sdk must own and nothing else: the runtime comes up
// through the SDK's crt0 and runtime_init exactly as it does for any SDK program, then the shell
// launches the second core, reads the resolved clocks, and hands each core to Rust -- core 0
// through light_app_main, core 1 through light_app_core1_main -- and does not get either back.
// What the shell exports TO Rust is the handful of functions below; keeping them in one file makes
// the size of that surface visible.
//
// THE SHELL HAS NO STDIO AND NO USB. The console, on both its transports, is Rust's
// (light_rp2::shell, on core 1): the USB device stack with the CDC class on it, from the
// controller register up, and the UART; the USB host role (light_rp2::usb_host, on core 0) is
// Rust's too. What stays here is what only the SDK can do -- boot, clocks, multicore launch, the
// bootrom (entering BOOTSEL, reading the BOOTSEL button), the SDK's own panic hook (which hands
// its message to the Rust relay), and the hard-fault handler.
#include <stdarg.h>
#include <stddef.h>
#include <stdio.h>

#include <hardware/clocks.h>
#include <hardware/structs/ioqspi.h>
#include <hardware/structs/sio.h>
#include <hardware/sync.h>
#include <boot/picobin.h>
#include <boot/picoboot_constants.h>
#include <pico/bootrom.h>
#include <pico/multicore.h>
#include <pico/stdlib.h>

// what the shell knows and Rust must not assume: the clocks the SDK runtime configured
struct light_shell_info {
        uint32_t clk_sys_hz;
        uint32_t clk_peri_hz;
};

// the Rust side: the board's entry points, and the port's panic relay
extern void light_app_main(const struct light_shell_info *info) __attribute__((noreturn));
extern void light_app_core1_main(const struct light_shell_info *info) __attribute__((noreturn));
extern void light_app_panic(const char *msg, size_t len) __attribute__((noreturn));

static volatile bool core1_ready = false;
static struct light_shell_info info;

//   enter the BOOTSEL bootloader through the bootrom, so a reflash needs no button. The Rust
// console calls this when the host asks for it (the 1200-baud line coding), and a device-role
// panic ends here
void __attribute__((noreturn)) light_shell_reset_to_bootsel(void)
{
        reset_usb_boot(0, 0);
        __breakpoint();
        while (true)
                tight_loop_contents();
}

//   the BOOTSEL button, read at runtime as an input: the board's one button that needs no GPIO
// wired for it. BOOTSEL shares the flash chip-select (QSPI_SS), so reading it means briefly
// floating that pin and sampling it -- which cannot touch flash while it happens, so this routine
// runs from RAM with interrupts off and no call into flash. The pin idles high (pulled up) and the
// button pulls it low, so pressed is the low reading. Restores chip-select before returning, or the
// next flash fetch would fault. Chip-independent: the QSPI_SS bit differs on RP2040 vs RP2350.
//
//   AND THE OTHER CORE MUST NOT FETCH FROM FLASH MEANWHILE. Core 1 runs the console from flash;
// a cache miss while chip-select floats fetches garbage -- a literal pool read that hands the
// console loop a pointer made of a spin count, a hard fault, and a console that silently died
// (found on a board polling this at 20 Hz: the death landed on whichever log line first missed
// the cache). So the read is bracketed by the SDK's multicore lockout, the same mechanism its own
// flash writes use: core 1 is asked over the FIFO to park in RAM, and released after.
//   NOT INLINE, AND THE "NOT" IS LOAD-BEARING. Placing a function in RAM says where its symbol
// goes; it says nothing about copies the compiler makes of its body. Inlined into a caller that
// lives in flash -- which its caller below does -- the whole careful dance above ends up being
// FETCHED FROM THE FLASH IT HAS JUST DISCONNECTED, and the core takes an instruction fetch from
// a chip that is not answering: an undefined instruction and a bus error, escalated to a fault
// it cannot stack a frame for, which is a locked-up core with no console and nothing on the
// display. It survives a light optimisation level only because the compiler happens not to
// inline it there; turn optimisation up, or turn on link-time optimisation, and it stops being
// a matter of happening to.
static bool __no_inline_not_in_flash_func(bootsel_sample)(void)
{
        const uint cs_index = 1; // QSPI_SS is the second QSPI IO
        uint32_t flags = save_and_disable_interrupts();
        hw_write_masked(&ioqspi_hw->io[cs_index].ctrl, GPIO_OVERRIDE_LOW << IO_QSPI_GPIO_QSPI_SS_CTRL_OEOVER_LSB, IO_QSPI_GPIO_QSPI_SS_CTRL_OEOVER_BITS);
        for (volatile int i = 0; i < 1000; ++i) {
                __nop();
        }
#ifdef __ARM_ARCH_6M__ // RP2040 (Cortex-M0+)
        const uint32_t cs_bit = 1u << 1;
#else // RP2350 (Cortex-M33 / Hazard3)
        const uint32_t cs_bit = SIO_GPIO_HI_IN_QSPI_CSN_BITS;
#endif
        bool pressed = (sio_hw->gpio_hi_in & cs_bit) == 0;
        hw_write_masked(&ioqspi_hw->io[cs_index].ctrl, GPIO_OVERRIDE_NORMAL << IO_QSPI_GPIO_QSPI_SS_CTRL_OEOVER_LSB, IO_QSPI_GPIO_QSPI_SS_CTRL_OEOVER_BITS);
        restore_interrupts(flags);
        return pressed;
}

//   WHAT THE BOOT ROM DID, and why. On a chip that chooses between images the question "which
// one am I?" has an answer only the ROM holds: which partition it booted, whether that image is
// still on probation, and -- when it refused one -- a diagnostic word saying what it made of the
// partition it was asked about. Reading it is a bootrom call, so it belongs here with the rest of
// the ROM surface. Zero means the ROM did not answer, which is every chip without this facility.
uint32_t light_shell_boot_info(uint32_t *out_diagnostic, uint32_t *out_params)
{
#if PICO_RP2350
        boot_info_t info;
        if (!rom_get_boot_info(&info))
                return 0;
        if (out_diagnostic)
                *out_diagnostic = info.boot_diagnostic;
        if (out_params) {
                out_params[0] = info.reboot_params[0];
                out_params[1] = info.reboot_params[1];
        }
        return info.boot_word;
#else
        (void) out_diagnostic;
        (void) out_params;
        return 0;
#endif
}

//   WHERE THE ASSETS LIVE, for firmware that keeps them out of its own image. The flash map says
// which region a product set aside for data, and only the boot ROM can be asked -- the map was
// read at boot and is not in this image. Answers the region's byte offset from the start of
// storage and its size, or false when this device has no such region: no map, or a map with
// nothing in it that accepts data.
//
//   The region is found by what it ACCEPTS rather than by name or number, because that is what
// decides where a download of assets lands. A map with two of them is a map that cannot say where
// assets go, so the first is taken and the rest ignored.
bool light_shell_data_region(uint32_t *out_offset, uint32_t *out_size)
{
#if PICO_RP2350
        for (unsigned i = 0; i < PARTITION_TABLE_MAX_PARTITIONS; i++) {
                uint32_t out[4];
                int rc = rom_get_partition_table_info(out, count_of(out),
                        PT_INFO_PARTITION_LOCATION_AND_FLAGS | PT_INFO_SINGLE_PARTITION | (i << 24));
                //   the ROM stops answering once the number runs past the map, which is how the
                // loop finds its end without asking how long the map is
                if (rc != 3)
                        break;
                uint32_t location = out[1];
                uint32_t flags = out[2];
                if (!(flags & PICOBIN_PARTITION_FLAGS_ACCEPTS_DEFAULT_FAMILY_DATA_BITS))
                        continue;
                uint32_t first = (location & PICOBIN_PARTITION_LOCATION_FIRST_SECTOR_BITS) >> PICOBIN_PARTITION_LOCATION_FIRST_SECTOR_LSB;
                uint32_t last = (location & PICOBIN_PARTITION_LOCATION_LAST_SECTOR_BITS) >> PICOBIN_PARTITION_LOCATION_LAST_SECTOR_LSB;
                if (out_offset)
                        *out_offset = first * 0x1000u;
                if (out_size)
                        *out_size = (last + 1 - first) * 0x1000u;
                return true;
        }
        return false;
#else
        (void) out_offset;
        (void) out_size;
        return false;
#endif
}

//   REPLACING THIS DEVICE'S OWN FIRMWARE. The four calls below are the whole of it, and every one
// of them is a boot ROM routine, so they live here with the rest of that surface.
#if PICO_RP2350
//   security level: this is the image the map trusts, so it writes with the permissions the map
// grants it. Addresses are STORAGE offsets and not the addresses the running image sees -- the
// slot being written is, by definition, the one the address window does not cover
#define LIGHT_CFLASH_FLAGS(op) ((cflash_flags_t) { \
        .flags = (CFLASH_SECLEVEL_VALUE_SECURE << CFLASH_SECLEVEL_LSB) | \
                 (CFLASH_ASPACE_VALUE_STORAGE << CFLASH_ASPACE_LSB) | \
                 ((op) << CFLASH_OP_LSB) })

//   WHICH SLOT IS NOT RUNNING, which is the only slot it is safe to write. The ROM knows which
// partition this image booted from; the map says which is its pair. Answers false on a device
// with no such pair -- one image, one place, and nowhere to put a new one while this one runs.
bool light_shell_update_slot(uint32_t *out_offset, uint32_t *out_size)
{
        boot_info_t info;
        if (!rom_get_boot_info(&info))
                return false;
        int running = (int8_t) ((info.boot_word >> 16) & 0xff);
        if (running < 0)
                return false;

        //   the pair, from whichever end of it is running. The ROM answers "the B of this A"
        // directly; for a B there is no such question to ask, so the map is searched for the A
        // that claims it
        int other = rom_get_b_partition((unsigned) running);
        if (other < 0) {
                other = -1;
                for (unsigned i = 0; i < PARTITION_TABLE_MAX_PARTITIONS; i++) {
                        if (rom_get_b_partition(i) == running) {
                                other = (int) i;
                                break;
                        }
                }
                if (other < 0)
                        return false;
        }

        uint32_t out[4];
        int rc = rom_get_partition_table_info(out, count_of(out),
                PT_INFO_PARTITION_LOCATION_AND_FLAGS | PT_INFO_SINGLE_PARTITION | ((unsigned) other << 24));
        if (rc != 3)
                return false;
        uint32_t location = out[1];
        uint32_t first = (location & PICOBIN_PARTITION_LOCATION_FIRST_SECTOR_BITS) >> PICOBIN_PARTITION_LOCATION_FIRST_SECTOR_LSB;
        uint32_t last = (location & PICOBIN_PARTITION_LOCATION_LAST_SECTOR_BITS) >> PICOBIN_PARTITION_LOCATION_LAST_SECTOR_LSB;
        if (out_offset)
                *out_offset = first * 0x1000u;
        if (out_size)
                *out_size = (last + 1 - first) * 0x1000u;
        return true;
}

//   Erase, program or read a range of storage, checked against the map's permissions so a write
// that strays outside the slot is refused rather than performed. The ROM parks the other core for
// the duration -- storage is not readable while it is being written, and the other core is running
// code out of it.
int light_shell_flash_erase(uint32_t offset, uint32_t len)
{
        return rom_flash_op(LIGHT_CFLASH_FLAGS(CFLASH_OP_VALUE_ERASE), XIP_BASE + offset, len, NULL);
}

int light_shell_flash_program(uint32_t offset, const uint8_t *buf, uint32_t len)
{
        return rom_flash_op(LIGHT_CFLASH_FLAGS(CFLASH_OP_VALUE_PROGRAM), XIP_BASE + offset, len, (uint8_t *) buf);
}

int light_shell_flash_read(uint32_t offset, uint8_t *buf, uint32_t len)
{
        return rom_flash_op(LIGHT_CFLASH_FLAGS(CFLASH_OP_VALUE_READ), XIP_BASE + offset, len, buf);
}

//   Start what was just written. This is not an ordinary reboot: it names the window the update
// went into, which is how the bootloader is told to prefer that slot over the comparison it would
// otherwise make. Does not return when it works.
int light_shell_reboot_update(uint32_t offset)
{
        return rom_reboot(REBOOT2_FLAG_REBOOT_TYPE_FLASH_UPDATE | REBOOT2_FLAG_NO_RETURN_ON_SUCCESS,
                10, XIP_BASE + offset, 0);
}

//   Keep an image that was started on approval. The scratch space is the CALLER'S, because it is
// four kilobytes and only a product that puts its images on probation ever needs it.
int light_shell_commit(uint8_t *scratch, uint32_t scratch_len)
{
        return rom_explicit_buy(scratch, scratch_len);
}
#else
bool light_shell_update_slot(uint32_t *out_offset, uint32_t *out_size)
{
        (void) out_offset;
        (void) out_size;
        return false;
}

int light_shell_flash_erase(uint32_t offset, uint32_t len)
{
        (void) offset;
        (void) len;
        return -1;
}

int light_shell_flash_program(uint32_t offset, const uint8_t *buf, uint32_t len)
{
        (void) offset;
        (void) buf;
        (void) len;
        return -1;
}

int light_shell_flash_read(uint32_t offset, uint8_t *buf, uint32_t len)
{
        (void) offset;
        (void) buf;
        (void) len;
        return -1;
}

int light_shell_reboot_update(uint32_t offset)
{
        (void) offset;
        return -1;
}

int light_shell_commit(uint8_t *scratch, uint32_t scratch_len)
{
        (void) scratch;
        (void) scratch_len;
        return -1;
}
#endif

bool light_shell_bootsel(void)
{
        multicore_lockout_start_blocking();
        bool pressed = bootsel_sample();
        multicore_lockout_end_blocking();
        return pressed;
}

//   the SDK's own panics -- assertions, spinlock misuse -- installed as PICO_PANIC_FUNCTION:
// formatted here, then handed to the Rust relay, which prints them over the console it owns and
// finishes the panic. Rust's own panics take the same relay directly, so every panic on the board
// -- whichever side and core raises it -- ends the same way
#define PANIC_MESSAGE_MAX 256
static char panic_message[PANIC_MESSAGE_MAX];

void __attribute__((noreturn)) light_shell_panic_sdk(const char *fmt, ...)
{
        va_list args;
        va_start(args, fmt);
        int n = vsnprintf(panic_message, sizeof panic_message, fmt ? fmt : "(no message)", args);
        va_end(args);
        if (n < 0)
                n = 0;
        if (n >= (int) sizeof panic_message)
                n = sizeof panic_message - 1;
        light_app_panic(panic_message, (size_t) n);
}

//   A HARD FAULT RECORDS ITSELF. The SDK's default handler breakpoints, which with no debugger
// attached is a second fault inside the first -- a lockup that leaves nothing behind but a PC of
// 0xFFFFFFFE. This one copies the stacked frame and the faulting core into a static, hands the
// fact to the panic relay (so the other core's console can say so), and spins where a debugger
// can read it all: light_shell_fault[core], valid when .magic is 0xFA17.
struct light_shell_fault_frame {
        uint32_t r0, r1, r2, r3, r12, lr, pc, xpsr;
        uint32_t magic;
};
struct light_shell_fault_frame light_shell_fault[2];

void __attribute__((used)) light_shell_hardfault(const uint32_t *frame)
{
        uint core = sio_hw->cpuid;
        struct light_shell_fault_frame *f = &light_shell_fault[core & 1];
        for (int i = 0; i < 8; ++i)
                ((uint32_t *) f)[i] = frame[i];
        f->magic = 0xFA17;
        static const char msg[] = "hard fault";
        light_app_panic(msg, sizeof msg - 1);
}

//   the frame is on whichever stack was active; both cores run on their main stacks here
void __attribute__((naked)) isr_hardfault(void)
{
        __asm volatile(
                "mrs r0, msp\n"
                "ldr r1, =light_shell_hardfault\n"
                "bx r1\n");
}

//   core 1 is handed to Rust once and never comes back: the console loop lives there. Ready means
// launched; the application need not wait for a host to open the console, since its early log
// lines wait in the bounded queue and are drained when one does
static void core1_main(void)
{
        //   answer core 0's lockout requests: the SDK's FIFO interrupt handler, in RAM, that
        // parks this core while core 0 floats the flash chip-select for the BOOTSEL read
        multicore_lockout_victim_init();
        core1_ready = true;
        light_app_core1_main(&info);
}

//   CORE 1'S STACK LIVES IN ORDINARY RAM, not the linker's SCRATCH_X default. SCRATCH_X
// sits directly below SCRATCH_Y (core 0's stack), and a deep core-0 call chain that dips
// past its own floor was found landing exactly on core 1's live frames: core 1 died
// alone, the application ran on, and the console -- which IS core 1 -- could not report
// its own death (diagnosed on the 3.49 with an on-screen core-1 heartbeat and a painted
// stack watermark: the heartbeat froze as the watermark hit zero). With core 1's stack
// here, SCRATCH_X is vacant runway: a core-0 excursion overwrites nothing that lives.
//
//   AND IT IS 8 KB, sized by the shell rather than by PICO_CORE1_STACK_SIZE (which also sizes
// the linker's SCRATCH_X reservation and cannot exceed 4 KB). The Rust console loop -- the USB
// device stack, its CDC class and the log formatting -- runs deeper than the 4 KB the C-era
// console needed: 5.3 KB measured on a 480x480 board through enumeration, so it overran that
// by more than a kilobyte into whatever .bss the linker placed below the array. On one board
// that was harmless; on another it was the scanout engine's DMA control word, and the board
// went dark with no console to say why. The array is painted before launch so the headroom can
// be read back (light_shell_core1_stack_free, reported once by the console after boot) and this
// size stays a measured one.
#define LIGHT_CORE1_STACK_SIZE 0x2000
#define STACK_PAINT 0xC1C1C1C1u
static uint32_t core1_stack[LIGHT_CORE1_STACK_SIZE / sizeof(uint32_t)];

//   how much of core 1's stack has never been touched: the painted words still standing from the
// bottom of the array. The stack grows down from the top, so this is the headroom under the
// deepest call so far
size_t light_shell_core1_stack_free(void)
{
        size_t words = 0;
        while (words < LIGHT_CORE1_STACK_SIZE / sizeof(uint32_t) && core1_stack[words] == STACK_PAINT)
                ++words;
        return words * sizeof(uint32_t);
}

size_t light_shell_core1_stack_size(void)
{
        return LIGHT_CORE1_STACK_SIZE;
}

int main(void)
{
        info.clk_sys_hz = clock_get_hz(clk_sys);
        info.clk_peri_hz = clock_get_hz(clk_peri);
        for (size_t i = 0; i < LIGHT_CORE1_STACK_SIZE / sizeof(uint32_t); ++i)
                core1_stack[i] = STACK_PAINT;
        //   core 1 first, so the console is running before the application's first log line.
        // Reset before launch, or a warm restart of core 0 hangs in the FIFO handshake
        multicore_reset_core1();
        multicore_launch_core1_with_stack(core1_main, core1_stack, sizeof(core1_stack));
        while (!core1_ready)
                tight_loop_contents();
        light_app_main(&info);
}

//   REBOOT ASKING ABOUT A PARTITION. The ROM's diagnostic word describes whatever partition it
// was ASKED about, and the asking is done by the reboot that precedes the boot: this reboots the
// normal way, naming the partition whose fate the next boot should report. Never returns when it
// succeeds; a failure returns the ROM's error so a caller can say so.
int light_shell_reboot_diagnosing(uint32_t partition)
{
#if PICO_RP2350
        return rom_reboot(REBOOT2_FLAG_REBOOT_TYPE_NORMAL | REBOOT2_FLAG_NO_RETURN_ON_SUCCESS, 10, partition, 0);
#else
        (void) partition;
        return -1;
#endif
}
