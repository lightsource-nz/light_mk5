// The C shell for the bare-CMSIS STM32 targets: what pico-sdk's runtime does for the RP2 boards,
// done here by the CMSIS startup file, ST's system file and the things this file adds -- the
// caches and clock tree where the chip has them -- before control passes to Rust and does not
// come back. THE SHELL HAS NO STDIO AND NO CONSOLE: the console, on every transport, is Rust's
// (light-shell-cmsis over the port's USB device controller, its USART and the ITM port). What
// crosses the boundary is one call with the measured clocks and a status string; there is no
// second core, and no callback back.
//
// Two chips so far. The H743 gets its caches and a 400 MHz clock tree with the 48 MHz USB clock
// off PLL3; the F411 gets a 72 MHz clock tree off its crystal with the 48 MHz USB clock off the
// PLL's Q output.
#include <stddef.h>
#include <stdint.h>

#if defined(STM32H743xx)
#include <stm32h7xx.h>
#elif defined(STM32F411xE)
#include <stm32f4xx.h>
#else
#error "no shell for this chip"
#endif

#include "shell.h"

struct light_shell_info {
        uint32_t clk_sys_hz;
        uint32_t clk_ahb_hz;
        uint32_t clk_apb2_hz;
        uint32_t clk_tim_hz;
        // what the clock tree did, NUL-terminated and static, for Rust to log once its console is up
        const char *clock_status;
};

extern void light_app_main(const struct light_shell_info *info) __attribute__((noreturn));

//   newlib's exit path, linked from crt0 and garbage-collected since main never returns, still
// references the file syscalls at link time; defining them here keeps libnosys's "will always
// fail" warnings out of every link. Nothing calls them: there is no stdio in this image
int _write(int fd, const void *buf, int len) { (void) fd; (void) buf; (void) len; return -1; }
int _read(int fd, void *buf, int len) { (void) fd; (void) buf; (void) len; return -1; }
int _close(int fd) { (void) fd; return -1; }
int _lseek(int fd, int off, int whence) { (void) fd; (void) off; (void) whence; return -1; }

#if defined(STM32H743xx)
//   the prescaler fields encode "divide at all" in the top bit and the power of two below it;
// SystemCoreClock is the CPU clock, already divided by D1CPRE, which has to be undone before HPRE
static uint32_t hclk_hz(void)
{
        static const uint8_t shift[16] = { 0,0,0,0,0,0,0,0, 1,2,3,4,6,7,8,9 };
        uint32_t d1cpre = (RCC->D1CFGR & RCC_D1CFGR_D1CPRE) >> RCC_D1CFGR_D1CPRE_Pos;
        uint32_t hpre = (RCC->D1CFGR & RCC_D1CFGR_HPRE) >> RCC_D1CFGR_HPRE_Pos;
        return (SystemCoreClock << shift[d1cpre & 0xF]) >> shift[hpre & 0xF];
}

static uint32_t apb_hz(uint32_t ppre_field)
{
        static const uint8_t shift[8] = { 0,0,0,0, 1,2,3,4 };
        return hclk_hz() >> shift[ppre_field & 7];
}
#else
static uint32_t apb_hz(uint32_t ppre_field)
{
        static const uint8_t shift[8] = { 0,0,0,0, 1,2,3,4 };
        return SystemCoreClock >> shift[ppre_field & 7];
}
#endif

int main(void)
{
        static struct light_shell_info info;
#if defined(STM32H743xx)
        //   the instruction cache before anything else: at 400 MHz flash is two wait states,
        // and fetch has no coherency problem to manage. The data cache stays OFF,
        // deliberately: the frame buffer is DMA territory one day, and a cached buffer handed to DMA
        // is silently wrong
        SCB_EnableICache();
        light_shell_clock_init();
        SystemCoreClockUpdate();
        //   keep the debug interface alive across WFI, or a running application becomes
        // unreachable over SWD ("Cortex-M CPUID: 0x0 is unrecognized")
        DBGMCU->CR |= DBGMCU_CR_DBG_SLEEPD1 | DBGMCU_CR_DBG_STOPD1 | DBGMCU_CR_DBG_STANDBYD1;
        uint32_t pclk1 = apb_hz((RCC->D2CFGR & RCC_D2CFGR_D2PPRE1) >> RCC_D2CFGR_D2PPRE1_Pos);
        info.clk_sys_hz = SystemCoreClock;
        info.clk_ahb_hz = hclk_hz();
        info.clk_apb2_hz = apb_hz((RCC->D2CFGR & RCC_D2CFGR_D2PPRE2) >> RCC_D2CFGR_D2PPRE2_Pos);
        // the APB1 timers run at twice APB1 whenever APB1 is prescaled (TIMPRE clear)
        info.clk_tim_hz = (pclk1 == hclk_hz()) ? pclk1 : pclk1 * 2;
#else
        light_shell_clock_init();
        SystemCoreClockUpdate();
        DBGMCU->CR |= DBGMCU_CR_DBG_SLEEP | DBGMCU_CR_DBG_STOP | DBGMCU_CR_DBG_STANDBY;
        // AHB at the core clock (HPRE is left at 1); APB1 halved, so its timers run at twice it
        uint32_t pclk1 = apb_hz((RCC->CFGR & RCC_CFGR_PPRE1) >> RCC_CFGR_PPRE1_Pos);
        info.clk_sys_hz = SystemCoreClock;
        info.clk_ahb_hz = SystemCoreClock;
        info.clk_apb2_hz = apb_hz((RCC->CFGR & RCC_CFGR_PPRE2) >> RCC_CFGR_PPRE2_Pos);
        info.clk_tim_hz = (pclk1 == SystemCoreClock) ? pclk1 : pclk1 * 2;
#endif
        info.clock_status = light_shell_clock_status();
        light_app_main(&info);
}
