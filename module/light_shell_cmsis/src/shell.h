#ifndef LIGHT_SHELL_CMSIS_H
#define LIGHT_SHELL_CMSIS_H

// the chip's clock tree, and afterwards a static string saying what happened -- which clock the
// core runs on and what fell back -- for the Rust side to log once its console is up. The H743
// runs 400 MHz off the crystal with the 48 MHz USB clock off PLL3; the F411 runs 72 MHz off its
// crystal with the 48 MHz USB clock off the PLL's Q output; both fall back to HSI
void light_shell_clock_init(void);
const char *light_shell_clock_status(void);

#endif
