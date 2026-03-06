#ifndef USB_DUMP_H
#define USB_DUMP_H

/**
 * @brief Initialize USB CDC-ACM endpoint for bulk SD card dumps.
 *
 * Enumerates as /dev/ttyACM* on Linux, /dev/tty.usbmodem* on macOS.
 * Accepts the same 6-byte storage command protocol as BLE:
 *   [0x00=READ][file_num][offset BE u32]
 * Responds with status byte, 4-byte LE file size, raw stream, 0x64 EOT.
 *
 * @return 0 on success, negative errno on failure
 */
int usb_dump_init(void);

#endif // USB_DUMP_H
