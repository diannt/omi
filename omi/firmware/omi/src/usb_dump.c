/*
 * USB CDC-ACM bulk dump endpoint for SD card audio data.
 *
 * Mirrors the BLE storage protocol over USB serial for ~3x faster transfers
 * without requiring the user to join a device WiFi SoftAP.
 *
 * Protocol (identical to storage.c:parse_storage_command):
 *   Host → Device (6 bytes): [cmd=0x00][file=0x01][offset BE u32]
 *   Device → Host (1 byte):  [status: 0=OK, 3=INVALID_FILE_SIZE, 4=ZERO_FILE_SIZE, 6=INVALID_COMMAND]
 *   Device → Host (4 bytes): [file_size LE u32]  (only if status==0)
 *   Device → Host:           [raw stream, file_size-offset bytes]
 *   Device → Host (1 byte):  [0x64 EOT]
 */

#include "usb_dump.h"

#include <zephyr/device.h>
#include <zephyr/drivers/uart.h>
#include <zephyr/kernel.h>
#include <zephyr/logging/log.h>
#include <zephyr/sys/ring_buffer.h>
#include <zephyr/usb/usb_device.h>

#include "lib/core/sd_card.h"

LOG_MODULE_REGISTER(usb_dump, CONFIG_LOG_DEFAULT_LEVEL);

// Protocol constants (mirror storage.c)
#define READ_COMMAND       0
#define INVALID_FILE_SIZE  3
#define ZERO_FILE_SIZE     4
#define INVALID_COMMAND    6
#define EOT_MARKER         100

#define CMD_LEN   6
#define CHUNK_LEN 4096

static const struct device *cdc_dev;

// RX ring buffer for incoming command bytes
RING_BUF_DECLARE(usb_rx_rb, 64);

// TX chunk buffer (static — do NOT allocate 1GB in RAM)
static uint8_t tx_chunk[CHUNK_LEN];

// Semaphore signalled when a full 6-byte command has been received
static K_SEM_DEFINE(cmd_ready_sem, 0, 1);
static uint8_t cmd_buf[CMD_LEN];
static volatile uint8_t cmd_buf_pos = 0;

// Blocking CDC write — uart_fifo_fill may accept fewer bytes than requested.
static int cdc_write_blocking(const uint8_t *buf, size_t len)
{
    size_t sent = 0;
    while (sent < len) {
        int n = uart_fifo_fill(cdc_dev, buf + sent, len - sent);
        if (n < 0) {
            return n;
        }
        if (n == 0) {
            // TX FIFO full — yield so USB stack drains it
            k_msleep(1);
            continue;
        }
        sent += n;
    }
    return 0;
}

static void usb_dump_uart_isr(const struct device *dev, void *user_data)
{
    ARG_UNUSED(user_data);

    if (!uart_irq_update(dev)) {
        return;
    }

    while (uart_irq_rx_ready(dev)) {
        uint8_t byte;
        if (uart_fifo_read(dev, &byte, 1) != 1) {
            break;
        }
        if (cmd_buf_pos < CMD_LEN) {
            cmd_buf[cmd_buf_pos++] = byte;
            if (cmd_buf_pos == CMD_LEN) {
                k_sem_give(&cmd_ready_sem);
            }
        }
        // Extra bytes silently dropped until buffer reset
    }
}

static void send_status(uint8_t code)
{
    cdc_write_blocking(&code, 1);
}

static void handle_read_command(uint32_t req_offset)
{
    uint32_t file_size = get_file_size();

    if (file_size == 0) {
        LOG_WRN("usb_dump: file size is 0");
        send_status(ZERO_FILE_SIZE);
        return;
    }
    if (req_offset >= file_size) {
        LOG_WRN("usb_dump: offset %u >= file_size %u", req_offset, file_size);
        send_status(INVALID_FILE_SIZE);
        return;
    }

    // OK — send status, then 4-byte LE file_size header
    send_status(0);
    uint8_t size_hdr[4] = {
        (uint8_t)(file_size & 0xFF),
        (uint8_t)((file_size >> 8) & 0xFF),
        (uint8_t)((file_size >> 16) & 0xFF),
        (uint8_t)((file_size >> 24) & 0xFF),
    };
    cdc_write_blocking(size_hdr, 4);

    // Stream file contents in CHUNK_LEN chunks
    uint32_t offset = req_offset;
    uint32_t remaining = file_size - offset;
    LOG_INF("usb_dump: streaming %u bytes from offset %u", remaining, offset);

    while (remaining > 0) {
        uint32_t chunk = remaining > CHUNK_LEN ? CHUNK_LEN : remaining;
        int r = read_audio_data(tx_chunk, chunk, offset);
        if (r < 0) {
            LOG_ERR("usb_dump: read_audio_data failed at offset %u: %d", offset, r);
            break;
        }
        if (cdc_write_blocking(tx_chunk, chunk) < 0) {
            LOG_ERR("usb_dump: CDC TX failed at offset %u", offset);
            break;
        }
        offset += chunk;
        remaining -= chunk;
    }

    // EOT marker
    uint8_t eot = EOT_MARKER;
    cdc_write_blocking(&eot, 1);
    LOG_INF("usb_dump: transfer complete, %u bytes sent", file_size - req_offset - remaining);
}

static void usb_dump_thread(void *p1, void *p2, void *p3)
{
    ARG_UNUSED(p1);
    ARG_UNUSED(p2);
    ARG_UNUSED(p3);

    uint32_t dtr = 0;

    // Wait for host to open the port (sets DTR)
    LOG_INF("usb_dump: waiting for DTR...");
    while (1) {
        uart_line_ctrl_get(cdc_dev, UART_LINE_CTRL_DTR, &dtr);
        if (dtr) {
            break;
        }
        k_msleep(100);
    }
    LOG_INF("usb_dump: DTR asserted, host connected");

    // Command loop
    while (1) {
        cmd_buf_pos = 0;
        k_sem_reset(&cmd_ready_sem);

        if (k_sem_take(&cmd_ready_sem, K_FOREVER) != 0) {
            continue;
        }

        uint8_t cmd = cmd_buf[0];
        uint8_t file_num = cmd_buf[1];
        uint32_t req_offset = ((uint32_t)cmd_buf[2] << 24) | ((uint32_t)cmd_buf[3] << 16) |
                              ((uint32_t)cmd_buf[4] << 8) | (uint32_t)cmd_buf[5];

        LOG_INF("usb_dump: cmd=%u file=%u offset=%u", cmd, file_num, req_offset);

        if (file_num != 1) {
            send_status(INVALID_FILE_SIZE);
            continue;
        }
        if (cmd != READ_COMMAND) {
            send_status(INVALID_COMMAND);
            continue;
        }

        handle_read_command(req_offset);
    }
}

#define USB_DUMP_STACK_SIZE 4096
#define USB_DUMP_PRIORITY   7
K_THREAD_STACK_DEFINE(usb_dump_stack, USB_DUMP_STACK_SIZE);
static struct k_thread usb_dump_thread_data;

int usb_dump_init(void)
{
    cdc_dev = DEVICE_DT_GET(DT_NODELABEL(cdc_acm_uart0));
    if (!device_is_ready(cdc_dev)) {
        LOG_ERR("usb_dump: CDC ACM device not ready");
        return -ENODEV;
    }

    int ret = usb_enable(NULL);
    if (ret != 0 && ret != -EALREADY) {
        LOG_ERR("usb_dump: usb_enable failed: %d", ret);
        return ret;
    }

    uart_irq_callback_set(cdc_dev, usb_dump_uart_isr);
    uart_irq_rx_enable(cdc_dev);

    k_thread_create(&usb_dump_thread_data, usb_dump_stack, USB_DUMP_STACK_SIZE,
                    usb_dump_thread, NULL, NULL, NULL,
                    USB_DUMP_PRIORITY, 0, K_NO_WAIT);
    k_thread_name_set(&usb_dump_thread_data, "usb_dump");

    LOG_INF("usb_dump: initialized");
    return 0;
}
