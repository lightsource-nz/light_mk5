// clock_f4.c -- the STM32F411's clock tree: 72 MHz off the board's 25 MHz crystal, with the
// 48 MHz USB clock off the PLL's Q output. ST's SystemInit() leaves the part on HSI at 16 MHz
// with every prescaler at 1; USB needs a 48 MHz clock that only the PLL can make.
//
// One set of PLL numbers serves both sources: the input divider brings either the crystal or
// HSI to 1 MHz, then x144 / 2 = 72 MHz for the core and / 3 = 48 MHz for USB. So a missing
// crystal is a USB clock at HSI's tolerance (out of spec, reported), not a different clock tree.
// 72 and not 100: two flash wait states at 3.3 V, no voltage-scaling step, and APB1 within its
// 50 MHz ceiling at half the core clock.
//
// ORDER IS LOAD-BEARING: flash wait states before the clock that needs them, prescalers before
// the switch. Every wait is bounded so a missing crystal is a slow board, not a dead one.
#include <stdbool.h>
#include <stdint.h>

#include <stm32f4xx.h>

#include "shell.h"

#define PLL_DIVM_HSE            25u
#define PLL_DIVM_HSI            16u
#define PLL_DIVN                144u
#define PLL_DIVP                2u              // field value 0 = /2
#define PLL_DIVQ                3u
#define CLOCK_WAIT_SPINS        1000000u

static const char *clock_status = ": HSI 16 MHz (clock init did not run)";

const char *light_shell_clock_status(void)
{
        return clock_status;
}

static bool wait_for(volatile uint32_t *reg, uint32_t mask, uint32_t spins)
{
        while (spins--) {
                if (*reg & mask)
                        return true;
        }
        return false;
}

void light_shell_clock_init(void)
{
        uint32_t divm = PLL_DIVM_HSE;
        uint32_t src = RCC_PLLCFGR_PLLSRC_HSE;
        bool hse = false;
        RCC->CR |= RCC_CR_HSEON;
        if (wait_for(&RCC->CR, RCC_CR_HSERDY, CLOCK_WAIT_SPINS)) {
                hse = true;
        } else {
                RCC->CR &= ~RCC_CR_HSEON;
                divm = PLL_DIVM_HSI;
                src = RCC_PLLCFGR_PLLSRC_HSI;
        }
        RCC->PLLCFGR = src
                        | (divm << RCC_PLLCFGR_PLLM_Pos)
                        | (PLL_DIVN << RCC_PLLCFGR_PLLN_Pos)
                        | (((PLL_DIVP / 2u) - 1u) << RCC_PLLCFGR_PLLP_Pos)
                        | (PLL_DIVQ << RCC_PLLCFGR_PLLQ_Pos);
        RCC->CR |= RCC_CR_PLLON;
        if (!wait_for(&RCC->CR, RCC_CR_PLLRDY, CLOCK_WAIT_SPINS)) {
                clock_status = ": PLL did not lock, staying on HSI at 16 MHz (no USB clock)";
                return;
        }
        // flash wait states before the switch: 72 MHz at 3.3 V needs 2, with the caches on
        FLASH->ACR = FLASH_ACR_LATENCY_2WS | FLASH_ACR_PRFTEN | FLASH_ACR_ICEN | FLASH_ACR_DCEN;
        // prescalers before the switch: AHB and APB2 at the core clock, APB1 halved
        RCC->CFGR = (RCC->CFGR & ~(RCC_CFGR_HPRE | RCC_CFGR_PPRE1 | RCC_CFGR_PPRE2))
                        | RCC_CFGR_HPRE_DIV1 | RCC_CFGR_PPRE1_DIV2 | RCC_CFGR_PPRE2_DIV1;
        RCC->CFGR = (RCC->CFGR & ~RCC_CFGR_SW) | RCC_CFGR_SW_PLL;
        if (!wait_for(&RCC->CFGR, RCC_CFGR_SWS_PLL, CLOCK_WAIT_SPINS)) {
                clock_status = ": system clock did not switch to the PLL";
                return;
        }
        clock_status = hse ? ", PLL 72 MHz off HSE, 48 MHz for USB" : ", PLL 72 MHz off HSI (no crystal; USB clock out of tolerance)";
}
