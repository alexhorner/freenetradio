/* CH32V307VCT6 memory map for FreeNetRadio: 192K flash + 128K RAM.
 *
 * The CH32V307 splits its on-chip SRAM between zero-wait flash and RAM via
 * the SRAM_CODE_MODE option bits (factory setting 288K/32K). Set the chip to
 * 192K/128K once with tools/ram-split (TARGET_MODE = 0b00) before flashing.
 */
MEMORY
{
    /* Zero-wait flash: copied to fast SRAM at boot. Hot code and data. */
    FLASH (rx)  : ORIGIN = 0x00000000, LENGTH = 192K /* boot alias of 0x08000000 */
    /* The rest of the 480K flash runs with wait states: cold code and data
       (see freenetradio.x). It ends where the presets' 32K begins (presets.rs). */
    SLOW  (rx)  : ORIGIN = 0x00030000, LENGTH = 256K /* to 0x70000 */
    RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 128K
}

REGION_ALIAS("REGION_TEXT", FLASH);
REGION_ALIAS("REGION_RODATA", FLASH);
REGION_ALIAS("REGION_DATA", RAM);
REGION_ALIAS("REGION_BSS", RAM);
REGION_ALIAS("REGION_HEAP", RAM);
REGION_ALIAS("REGION_STACK", RAM);
