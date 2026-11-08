// WerewolfOS window viewer.
//
// Maps the ivshmem file the guest draws into and shows it in a Qt window.
// Input (keys, wheel) goes back through the shared input ring. With
// --snapshot it reads one frame headlessly and writes a PNG, which is what
// CI and the QEMU smoke test use.
//
// SPDX-License-Identifier: MIT

#include <QApplication>
#include <QElapsedTimer>
#include <QFile>
#include <QImage>
#include <QKeyEvent>
#include <QPainter>
#include <QThread>
#include <QTimer>
#include <QWheelEvent>
#include <QWidget>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

namespace {

constexpr uint32_t kMagic = 0x57494E31; // "WIN1"
constexpr uint32_t kFormatRgba8888 = 1;

constexpr uint64_t kFieldMagic = 0x00;
constexpr uint64_t kFieldWidth = 0x08;
constexpr uint64_t kFieldHeight = 0x0C;
constexpr uint64_t kFieldStride = 0x10;
constexpr uint64_t kFieldFormat = 0x14;
constexpr uint64_t kFieldFrameSeq = 0x18;
constexpr uint64_t kFieldInputHead = 0x20;
constexpr uint64_t kFieldInputTail = 0x24;
constexpr uint64_t kPixels = 0x1000;
constexpr uint64_t kInputRing = 0x0040;
constexpr uint32_t kInputSlots = 64;
constexpr uint64_t kInputEntry = 8;

enum InputKind : uint8_t { kInputKey = 1, kInputScroll = 2, kInputClick = 3 };

uint32_t readU32(const uint8_t *base, uint64_t offset) {
    uint32_t value;
    std::memcpy(&value, base + offset, sizeof(value));
    return value;
}

void writeU32(uint8_t *base, uint64_t offset, uint32_t value) {
    std::memcpy(base + offset, &value, sizeof(value));
}

void pushInput(uint8_t *base, uint8_t kind, uint8_t value, uint16_t key, uint16_t x, uint16_t y) {
    uint32_t head = readU32(base, kFieldInputHead);
    uint32_t tail = readU32(base, kFieldInputTail);
    if (head - tail >= kInputSlots) {
        return; // ring full: dropping input is better than stalling the UI
    }
    uint8_t entry[kInputEntry] = {
        kind,
        value,
        static_cast<uint8_t>(key & 0xFF),
        static_cast<uint8_t>(key >> 8),
        static_cast<uint8_t>(x & 0xFF),
        static_cast<uint8_t>(x >> 8),
        static_cast<uint8_t>(y & 0xFF),
        static_cast<uint8_t>(y >> 8),
    };
    std::memcpy(base + kInputRing + (head % kInputSlots) * kInputEntry, entry, sizeof(entry));
    writeU32(base, kFieldInputHead, head + 1);
}

struct Shared {
    QFile file;
    uchar *data = nullptr;
    qsizetype size = 0;

    /// Opens the shared file, waiting for QEMU to create it. Starting the
    /// viewer before the VM is normal: the window appears once the file and
    /// the WIN1 header exist.
    bool open(const QString &path, int waitMilliseconds = 20000) {
        QElapsedTimer timer;
        timer.start();
        do {
            file.setFileName(path);
            if (file.open(QIODevice::ReadWrite)) {
                size = file.size();
                if (size > static_cast<qsizetype>(kPixels)) {
                    data = file.map(0, size);
                    if (data != nullptr) {
                        return true;
                    }
                }
                file.close();
            }
            data = nullptr;
            QThread::msleep(100);
        } while (timer.elapsed() < waitMilliseconds);
        return false;
    }

    bool waitReady(int waitMilliseconds = 20000) {
        QElapsedTimer timer;
        timer.start();
        do {
            if (u32(kFieldMagic) == kMagic) {
                return true;
            }
            QThread::msleep(100);
        } while (timer.elapsed() < waitMilliseconds);
        return false;
    }

    uint32_t u32(uint64_t offset) const {
        return readU32(reinterpret_cast<const uint8_t *>(data), offset);
    }
};

class Viewer : public QWidget {
public:
    Viewer(Shared &shared, QWidget *parent = nullptr) : QWidget(parent), m_shared(shared) {
        setWindowTitle("Wolfser — WerewolfOS");
        resize(900, 640);
        m_lastSequence = 0;
        auto *timer = new QTimer(this);
        connect(timer, &QTimer::timeout, this, [this] {
            if (m_shared.u32(kFieldMagic) != kMagic) {
                if (!m_sized) {
                    update(); // keep the waiting screen alive
                }
                return;
            }
            const uint32_t width = m_shared.u32(kFieldWidth);
            const uint32_t height = m_shared.u32(kFieldHeight);
            if (!m_sized && width != 0 && height != 0) {
                resize(static_cast<int>(width), static_cast<int>(height));
                m_sized = true;
            }
            const uint32_t sequence = m_shared.u32(kFieldFrameSeq);
            if (sequence != m_lastSequence) {
                m_lastSequence = sequence;
                update();
            }
        });
        timer->start(16);
    }

protected:
    void paintEvent(QPaintEvent *) override {
        if (m_shared.u32(kFieldMagic) != kMagic) {
            // The window is opened with QEMU, long before the guest starts
            // Wolfser. Show something useful instead of exiting.
            QPainter painter(this);
            painter.fillRect(rect(), QColor(30, 58, 95));
            QFont font = painter.font();
            font.setPointSize(16);
            font.setBold(true);
            painter.setFont(font);
            painter.setPen(Qt::white);
            painter.drawText(rect().adjusted(24, 0, -24, -24), Qt::AlignCenter,
                             "Waiting for Wolfser");
            font.setPointSize(10);
            font.setBold(false);
            painter.setFont(font);
            painter.drawText(rect().adjusted(24, 36, -24, 0), Qt::AlignHCenter | Qt::AlignBottom,
                             "type  wolfser  in the guest console");
            return;
        }
        const uint32_t width = m_shared.u32(kFieldWidth);
        const uint32_t height = m_shared.u32(kFieldHeight);
        const uint32_t stride = m_shared.u32(kFieldStride);
        if (width == 0 || height == 0 || stride < width * 4) {
            return;
        }
        QImage frame(m_shared.data + kPixels, static_cast<int>(width), static_cast<int>(height),
                     static_cast<int>(stride), QImage::Format_RGBA8888);
        QPainter painter(this);
        painter.drawImage(rect(), frame);
    }

    void keyPressEvent(QKeyEvent *event) override {
        uint16_t key = 0;
        switch (event->key()) {
        case Qt::Key_Up: key = 1; break;
        case Qt::Key_Down: key = 2; break;
        case Qt::Key_PageUp: key = 3; break;
        case Qt::Key_PageDown: key = 4; break;
        case Qt::Key_Home: key = 5; break;
        case Qt::Key_End: key = 6; break;
        case Qt::Key_Return:
        case Qt::Key_Enter: key = 10; break;
        case Qt::Key_Backspace: key = 11; break;
        case Qt::Key_Escape: key = 12; break;
        case Qt::Key_L:
            if (event->modifiers() & Qt::ControlModifier) {
                key = 13; // focus the address bar
                break;
            }
            if (event->text().size() == 1 && event->text().at(0).isPrint()) {
                key = static_cast<uint16_t>(event->text().at(0).unicode());
                break;
            }
            return;
        default:
            if (event->text().size() == 1 && event->text().at(0).isPrint()) {
                // Keep the case: URLs and search queries are byte-exact.
                key = static_cast<uint16_t>(event->text().at(0).unicode());
            } else {
                return;
            }
        }
        pushInput(m_shared.data, kInputKey, 0, key, 0, 0);
    }

    void wheelEvent(QWheelEvent *event) override {
        int delta = event->angleDelta().y() / 120;
        if (delta > 3) delta = 3;
        if (delta < -3) delta = -3;
        if (delta != 0) {
            pushInput(m_shared.data, kInputScroll, static_cast<uint8_t>(static_cast<int8_t>(delta)), 0, 0, 0);
        }
    }

private:
    Shared &m_shared;
    uint32_t m_lastSequence = 0;
    bool m_sized = false;
};

int snapshot(Shared &shared, const QString &path, int timeoutSeconds) {
    QElapsedTimer timer;
    timer.start();
    while (timer.elapsed() < timeoutSeconds * 1000) {
        if (shared.u32(kFieldMagic) == kMagic &&
            shared.u32(kFieldFrameSeq) != 0 &&
            shared.u32(kFieldFormat) == kFormatRgba8888) {
            const uint32_t width = shared.u32(kFieldWidth);
            const uint32_t height = shared.u32(kFieldHeight);
            const uint32_t stride = shared.u32(kFieldStride);
            if (width == 0 || height == 0 || stride < width * 4) {
                std::fprintf(stderr, "viewer: invalid geometry\n");
                return 2;
            }
            QImage frame(shared.data + kPixels, static_cast<int>(width), static_cast<int>(height),
                         static_cast<int>(stride), QImage::Format_RGBA8888);
            if (!frame.save(path)) {
                std::fprintf(stderr, "viewer: cannot write %s\n", path.toUtf8().constData());
                return 2;
            }
            size_t nonBackground = 0;
            for (uint32_t y = 0; y < height; y += 4) {
                const uchar *row = frame.constScanLine(static_cast<int>(y));
                for (uint32_t x = 0; x < width; x += 4) {
                    const uchar *pixel = row + x * 4;
                    if (pixel[0] < 240 || pixel[1] < 240 || pixel[2] < 240) {
                        ++nonBackground;
                    }
                }
            }
            std::printf("snapshot: %ux%u frame=%u nonwhite-samples=%zu -> %s\n", width, height,
                        shared.u32(kFieldFrameSeq), nonBackground, path.toUtf8().constData());
            return 0;
        }
        QCoreApplication::processEvents();
    }
    std::fprintf(stderr, "viewer: no frame within %d s (is Wolfser running?)\n",
                 timeoutSeconds);
    return 2;
}

} // namespace

int main(int argc, char **argv) {
    QString shmPath = "/dev/shm/werewolf-window";
    QString snapshotPath;
    int timeout = 20;
    int scalePercent = 100;
    bool headless = false;

    for (int i = 1; i < argc; ++i) {
        const std::string arg = argv[i];
        if (arg == "--shm" && i + 1 < argc) {
            shmPath = QString::fromLocal8Bit(argv[++i]);
        } else if (arg == "--snapshot" && i + 1 < argc) {
            snapshotPath = QString::fromLocal8Bit(argv[++i]);
            headless = true;
        } else if (arg == "--timeout" && i + 1 < argc) {
            timeout = std::atoi(argv[++i]);
        } else if (arg == "--scale" && i + 1 < argc) {
            scalePercent = std::atoi(argv[++i]);
        } else {
            std::fprintf(stderr,
                         "usage: %s [--shm PATH] [--snapshot OUT.png] [--timeout SECS] [--scale %%]\n",
                         argv[0]);
            return 1;
        }
    }

    if (headless) {
        qputenv("QT_QPA_PLATFORM", "offscreen");
    }
    QApplication app(argc, argv);

    Shared shared;
    if (!shared.open(shmPath, headless ? timeout * 1000 : 20000)) {
        std::fprintf(stderr, "viewer: cannot map %s\n", shmPath.toUtf8().constData());
        return 2;
    }

    if (headless) {
        return snapshot(shared, snapshotPath, timeout);
    }

    // No wait for the WIN1 header here: the window stays open and shows a
    // waiting screen until the guest starts Wolfser.

    Viewer viewer(shared);
    if (scalePercent != 100) {
        const int width = static_cast<int>(shared.u32(kFieldWidth)) * scalePercent / 100;
        const int height = static_cast<int>(shared.u32(kFieldHeight)) * scalePercent / 100;
        viewer.resize(width, height);
    }
    viewer.show();
    viewer.raise();
    viewer.activateWindow();
    return app.exec();
}
