// The bootloader: the first thing the hardware runs, and the only thing that chooses.
//
// The boot ROM verifies whatever it finds at the start of flash against a key hash held in
// one-time memory, and runs it. That is this image. Verifying an image is not the same as
// electing one of two, though, so a device with an A/B pair needs something to compare the slots
// and hand over to the better one -- and this is that, and nothing else.
//
// IT IS C, WHICH THE REST OF THE FRAMEWORK IS NOT. Everything above the shell is Rust; this is
// below even the shell. It runs before any runtime exists, consists entirely of boot ROM calls,
// and is the one image a device can never recover from by an update -- so it stays as small as it
// can be, with nothing in it that could have been left out. There is no console here on purpose:
// a bootloader that formats a log line is a bootloader with a formatter in it.
//
// THE FLASH MAP IS EMBEDDED IN THIS IMAGE (light_add_bootloader, from the layout the product
// supplies), so the map and the code that reads it are one signed artefact: a device cannot hold
// a map its bootloader disagrees with, and verifying this image verifies the map with it.
#include <pico/bootrom.h>
#include <boot/picobin.h>

//   the ROM needs scratch for the work it does on our behalf -- parsing the map, comparing the
// slots' images, verifying the one it picks. 4K is what its documented minimum (3.25K) rounds to
static __attribute__((aligned(4))) uint8_t workarea[4 * 1024];

//   flash sectors are 4K, and a partition's location is recorded in sectors from the start of
// flash. The window handed to the ROM is an address in the execute-in-place region
#define SECTOR_SIZE 0x1000u

//   WHY IT GAVE UP, for a board that has no console and no debugger of its own. The code is left
// at a fixed address before the bootloader hands itself back to the host's bootloader, which is
// the only recovery an undebugged device has. NOT a hang: a board that cannot boot must still be
// flashable. `stage` says which step refused, `rc` is what the ROM said about it.
#define GIVE_UP_REPORT ((volatile uint32_t *) 0x20081fe0)
#define GIVE_UP_MAGIC 0xB007FA11u

static __attribute__((noreturn)) void give_up(uint32_t stage, int rc)
{
        GIVE_UP_REPORT[0] = GIVE_UP_MAGIC;
        GIVE_UP_REPORT[1] = stage;
        GIVE_UP_REPORT[2] = (uint32_t) rc;
        reset_usb_boot(0, 0);
        while (true)
                tight_loop_contents();
}

int main(void)
{
        //   the map first: everything below is a question about partitions, and without it there
        // are none
        int rc = rom_load_partition_table(workarea, sizeof workarea, false);
        if (rc)
                give_up(1, rc);

        //   which of the A/B pair. The ROM does the comparing -- versions, signatures, whether an
        // image is on probation -- because it is the same judgement it makes when it boots one
        // itself, and a second implementation of that judgement is a second thing to get wrong.
        // The "during update" form is the one to use when a buy may be pending: it leaves an
        // in-progress update's bookkeeping intact, and it refuses a slot whose image is not valid
        int picked = rom_pick_ab_partition_during_update((uint32_t *) workarea, sizeof workarea, 0);
        if (picked < 0)
                give_up(2, picked);

        //   where that partition is. The ROM answers in sectors: first and last, inclusive
        rc = rom_get_partition_table_info((uint32_t *) workarea, 0x8,
                PT_INFO_PARTITION_LOCATION_AND_FLAGS | PT_INFO_SINGLE_PARTITION | ((unsigned) picked << 24));
        if (rc != 3)
                give_up(3, rc);
        uint32_t location = ((uint32_t *) workarea)[1];
        uint32_t first = (location & PICOBIN_PARTITION_LOCATION_FIRST_SECTOR_BITS) >> PICOBIN_PARTITION_LOCATION_FIRST_SECTOR_LSB;
        uint32_t last = (location & PICOBIN_PARTITION_LOCATION_LAST_SECTOR_BITS) >> PICOBIN_PARTITION_LOCATION_LAST_SECTOR_LSB;

        //   hand over. The ROM verifies the image in that window before it runs it, so the chain
        // of trust continues rather than ending here; it does not return if it succeeds
        rom_chain_image(workarea, sizeof workarea,
                XIP_BASE + first * SECTOR_SIZE, (last + 1 - first) * SECTOR_SIZE);

        //   a chain that returns is a slot whose image the ROM would not run
        give_up(4, 0);
}
