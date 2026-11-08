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
#include <QMouseEvent>
#include <QNetworkAccessManager>
#include <QNetworkReply>
#include <QNetworkRequest>
#include <QPainter>
#include <QThread>
#include <QTimer>
#include <QUrl>
#include <QWheelEvent>
#include <QWidget>

#include <atomic>
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
constexpr uint64_t kFieldWindowRequest = 0x2C;
constexpr uint64_t kFieldWebRequestSeq = 0x30;
constexpr uint64_t kFieldWebRequestLen = 0x34;
constexpr uint64_t kFieldWebDoneSeq = 0x38;
constexpr uint64_t kFieldWebStatus = 0x3C;
constexpr uint64_t kFieldWebBodyLen = 0x40;
constexpr uint64_t kFieldWebError = 0x44;
constexpr uint64_t kFieldWebFinalLen = 0x48;
constexpr uint64_t kFieldWebCapable = 0x4C;
constexpr uint64_t kFieldCursor = 0x50;
constexpr uint64_t kFieldImgRequestSeq = 0x54;
constexpr uint64_t kFieldImgDoneSeq = 0x58;
constexpr uint64_t kFieldImgWidth = 0x5C;
constexpr uint64_t kFieldImgHeight = 0x60;
constexpr uint64_t kFieldImgLen = 0x64;
constexpr uint64_t kFieldImgError = 0x68;
constexpr uint64_t kFieldImgRequestLen = 0x6C;
constexpr uint64_t kFieldImgMaxWidth = 0x70;
constexpr uint64_t kFieldImgMaxHeight = 0x74;
constexpr uint64_t kPixels = 0x1000;
constexpr uint64_t kWebRequest = 0x240000;
constexpr int kWebRequestMax = 1024;
constexpr uint64_t kWebFinalUrl = 0x240800;
constexpr int kWebFinalUrlMax = 1024;
constexpr uint64_t kWebBody = 0x241000;
constexpr int kWebBodyMax = 0x400000 - static_cast<int>(kWebBody);
constexpr uint64_t kImgUrl = 0x240C00;
constexpr int kImgUrlMax = 1024;
constexpr int kImageMaxWidth = 480;
constexpr int kImageMaxHeight = 360;
constexpr uint64_t kInputRing = 0x0100;
constexpr uint32_t kInputSlots = 64;
constexpr uint64_t kInputEntry = 8;

enum InputKind : uint8_t { kInputKey = 1, kInputScroll = 2, kInputClick = 3, kInputMove = 4 };

constexpr uint32_t kWebErrorNone = 0;
constexpr uint32_t kWebErrorNetwork = 1;
constexpr uint32_t kWebErrorUnsupported = 3;

uint32_t readU32(const uint8_t *base, uint64_t offset) {
    uint32_t value;
    std::memcpy(&value, base + offset, sizeof(value));
    return value;
}

void writeU32(uint8_t *base, uint64_t offset, uint32_t value) {
    std::memcpy(base + offset, &value, sizeof(value));
}

void pushInput(uint8_t *base, uint8_t kind, uint8_t value, uint16_t key, uint16_t x, uint16_t y) {
    uint32_t head = readU32(base, kFieldInputHead) % kInputSlots;
    uint32_t tail = readU32(base, kFieldInputTail) % kInputSlots;
    // Both indices are positions in the ring. A plain subtraction underflows
    // once head wraps behind tail, which used to look "full" and silently
    // dropped every event after 64 of them.
    const uint32_t queued = (head + kInputSlots - tail) % kInputSlots;
    if (queued >= kInputSlots - 1) {
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
    std::memcpy(base + kInputRing + head * kInputEntry, entry, sizeof(entry));
    writeU32(base, kFieldInputHead, (head + 1) % kInputSlots);
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

class WebProxy : public QObject {
public:
    explicit WebProxy(Shared &shared, QObject *parent = nullptr)
        : QObject(parent), m_shared(shared) {
        // Tell the guest that HTTPS (and redirects with any scheme) can be
        // fetched through this viewer; an old viewer leaves the flag at 0.
        writeU32(m_shared.data, kFieldWebCapable, 1);
        m_lastRequest = m_shared.u32(kFieldWebRequestSeq);
        m_lastImage = m_shared.u32(kFieldImgRequestSeq);
        auto *timer = new QTimer(this);
        connect(timer, &QTimer::timeout, this, [this] {
            tick();
            tickImages();
        });
        timer->start(40);
    }

private:
    void tick() {
        const uint32_t seq = m_shared.u32(kFieldWebRequestSeq);
        if (seq == 0 || seq == m_lastRequest) {
            return;
        }
        m_lastRequest = seq;
        std::fprintf(stderr, "proxy: request %u\n", seq);
        start(seq);
    }

    void tickImages() {
        const uint32_t seq = m_shared.u32(kFieldImgRequestSeq);
        if (seq == 0 || seq == m_lastImage) {
            return;
        }
        m_lastImage = seq;
        startImage(seq);
    }

    void start(uint32_t seq) {
        // A page navigation owns the body buffer; cancel any image that
        // could otherwise finish later and overwrite it.
        if (m_imageReply != nullptr) {
            m_imageSeq = 0;
            m_imageReply->abort();
            m_imageReply->deleteLater();
            m_imageReply = nullptr;
        }
        if (m_reply != nullptr) {
            m_reply->abort();
            m_reply->deleteLater();
            m_reply = nullptr;
        }
        uint32_t length = m_shared.u32(kFieldWebRequestLen);
        if (length > static_cast<uint32_t>(kWebRequestMax)) {
            length = kWebRequestMax;
        }
        const QString text = QString::fromUtf8(
            reinterpret_cast<const char *>(m_shared.data + kWebRequest),
            static_cast<int>(length));
        const QUrl url(text);
        if (!url.isValid() ||
            (url.scheme() != QLatin1String("https") && url.scheme() != QLatin1String("http"))) {
            publish(seq, 0, QByteArray(), kWebErrorUnsupported, text);
            return;
        }
        QNetworkRequest request(url);
        // The guest resolves redirects itself so it can choose the HTTP path
        // for http:// and the host path for https://.
        request.setAttribute(QNetworkRequest::RedirectPolicyAttribute,
                             QNetworkRequest::ManualRedirectPolicy);
        request.setRawHeader("Accept-Encoding", "identity");
        request.setHeader(QNetworkRequest::UserAgentHeader,
                          "Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Wolfser/0.3");
        std::fprintf(stderr, "proxy: fetching %s\n", url.toString().toUtf8().constData());
        m_reply = m_manager.get(request);
        connect(m_reply, &QNetworkReply::finished, this, [this, seq] { finished(seq); });
    }

    void finished(uint32_t seq) {
        QNetworkReply *reply = m_reply;
        if (reply == nullptr) {
            return;
        }
        m_reply = nullptr;
        const int status = reply->attribute(QNetworkRequest::HttpStatusCodeAttribute).toInt();
        const QUrl redirect =
            reply->attribute(QNetworkRequest::RedirectionTargetAttribute).toUrl();
        uint32_t error = kWebErrorNone;
        QByteArray body;
        if (status >= 300 && status < 400 && redirect.isValid()) {
            body = reply->url().resolved(redirect).toEncoded();
        } else {
            body = reply->readAll();
            if (status == 0 && reply->error() != QNetworkReply::NoError) {
                error = kWebErrorNetwork;
            }
        }
        const QString finalUrl = reply->url().toString();
        std::fprintf(stderr, "proxy: status=%d bytes=%lld error=%d url=%s\n", status,
                     static_cast<long long>(body.size()), static_cast<int>(reply->error()),
                     finalUrl.toUtf8().constData());
        reply->deleteLater();
        publish(seq, static_cast<uint32_t>(status), body, error, finalUrl);
    }

    void startImage(uint32_t seq) {
        if (m_imageReply != nullptr) {
            m_imageSeq = 0;
            m_imageReply->abort();
            m_imageReply->deleteLater();
            m_imageReply = nullptr;
        }
        if (m_reply != nullptr) {
            // A page fetch owns the body buffer; skip this image.
            publishImage(seq, 0, 0, 0, kWebErrorUnsupported);
            return;
        }
        uint32_t length = m_shared.u32(kFieldImgRequestLen);
        if (length > static_cast<uint32_t>(kImgUrlMax)) {
            length = kImgUrlMax;
        }
        const QString text = QString::fromUtf8(
            reinterpret_cast<const char *>(m_shared.data + kImgUrl),
            static_cast<int>(length));
        const QUrl url(text);
        if (!url.isValid() || (url.scheme() != QLatin1String("http") &&
                               url.scheme() != QLatin1String("https"))) {
            publishImage(seq, 0, 0, 0, kWebErrorUnsupported);
            return;
        }
        QNetworkRequest request(url);
        request.setAttribute(QNetworkRequest::RedirectPolicyAttribute,
                             QNetworkRequest::NoLessSafeRedirectPolicy);
        request.setRawHeader("Accept-Encoding", "identity");
        request.setHeader(QNetworkRequest::UserAgentHeader,
                          "Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Wolfser/0.3");
        std::fprintf(stderr, "proxy: image %s\n", url.toString().toUtf8().constData());
        m_imageSeq = seq;
        m_imageReply = m_manager.get(request);
        connect(m_imageReply, &QNetworkReply::finished, this, [this, seq] { finishedImage(seq); });
    }

    void finishedImage(uint32_t seq) {
        QNetworkReply *reply = m_imageReply;
        if (reply == nullptr || seq != m_imageSeq) {
            return;
        }
        m_imageReply = nullptr;
        const int status = reply->attribute(QNetworkRequest::HttpStatusCodeAttribute).toInt();
        QByteArray data = reply->readAll();
        const bool failed = (reply->error() != QNetworkReply::NoError && status == 0) ||
                            status >= 400;
        reply->deleteLater();
        if (failed) {
            publishImage(seq, 0, 0, 0, kWebErrorNetwork);
            return;
        }
        QImage image;
        if (!image.loadFromData(data) || image.isNull()) {
            publishImage(seq, 0, 0, 0, 2); // decode error
            return;
        }
        uint32_t maxWidth = m_shared.u32(kFieldImgMaxWidth);
        uint32_t maxHeight = m_shared.u32(kFieldImgMaxHeight);
        if (maxWidth == 0 || maxWidth > kImageMaxWidth) {
            maxWidth = kImageMaxWidth;
        }
        if (maxHeight == 0 || maxHeight > kImageMaxHeight) {
            maxHeight = kImageMaxHeight;
        }
        if (image.width() > static_cast<int>(maxWidth) ||
            image.height() > static_cast<int>(maxHeight)) {
            image = image.scaled(static_cast<int>(maxWidth), static_cast<int>(maxHeight),
                                 Qt::KeepAspectRatio, Qt::SmoothTransformation);
        }
        image = image.convertToFormat(QImage::Format_RGBA8888);
        const uint32_t width = static_cast<uint32_t>(image.width());
        const uint32_t height = static_cast<uint32_t>(image.height());
        const uint32_t stride = width * 4;
        const uint32_t total = stride * height;
        if (width == 0 || height == 0 || total > static_cast<uint32_t>(kWebBodyMax)) {
            publishImage(seq, 0, 0, 0, 2);
            return;
        }
        for (uint32_t y = 0; y < height; ++y) {
            std::memcpy(m_shared.data + kWebBody + static_cast<size_t>(y) * stride,
                        image.constScanLine(static_cast<int>(y)), stride);
        }
        std::fprintf(stderr, "proxy: image %ux%u bytes=%u\n", width, height, total);
        publishImage(seq, width, height, total, kWebErrorNone);
    }

    void publishImage(uint32_t seq, uint32_t width, uint32_t height, uint32_t length,
                      uint32_t error) {
        writeU32(m_shared.data, kFieldImgWidth, width);
        writeU32(m_shared.data, kFieldImgHeight, height);
        writeU32(m_shared.data, kFieldImgLen, length);
        writeU32(m_shared.data, kFieldImgError, error);
        std::atomic_thread_fence(std::memory_order_seq_cst);
        writeU32(m_shared.data, kFieldImgDoneSeq, seq);
    }

    void publish(uint32_t seq, uint32_t status, const QByteArray &body, uint32_t error,
                 const QString &finalUrl) {
        QByteArray final = finalUrl.toUtf8();
        if (final.size() > kWebFinalUrlMax) {
            final.truncate(kWebFinalUrlMax);
        }
        const QByteArray payload = body.left(kWebBodyMax);
        std::memcpy(m_shared.data + kWebBody, payload.constData(),
                    static_cast<size_t>(payload.size()));
        std::memcpy(m_shared.data + kWebFinalUrl, final.constData(),
                    static_cast<size_t>(final.size()));
        writeU32(m_shared.data, kFieldWebStatus, status);
        writeU32(m_shared.data, kFieldWebBodyLen, static_cast<uint32_t>(payload.size()));
        writeU32(m_shared.data, kFieldWebError, error);
        writeU32(m_shared.data, kFieldWebFinalLen, static_cast<uint32_t>(final.size()));
        // The done sequence is published last: the guest must see the body
        // and the status before it sees the completion marker.
        std::atomic_thread_fence(std::memory_order_seq_cst);
        writeU32(m_shared.data, kFieldWebDoneSeq, seq);
    }

    Shared &m_shared;
    QNetworkAccessManager m_manager;
    QNetworkReply *m_reply = nullptr;
    QNetworkReply *m_imageReply = nullptr;
    uint32_t m_lastRequest = 0;
    uint32_t m_lastImage = 0;
    uint32_t m_imageSeq = 0;
};

class Viewer : public QWidget {
public:
    Viewer(Shared &shared, QWidget *parent = nullptr) : QWidget(parent), m_shared(shared) {
        setWindowTitle("Wolfser — WerewolfOS");
        m_frameWidth = 900;
        m_frameHeight = 640;
        resize(900, 640);
        setMouseTracking(true);
        m_lastSequence = 0;
        m_hoverClock.start();
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
                m_frameWidth = static_cast<int>(width);
                m_frameHeight = static_cast<int>(height);
                applyZoom(m_zoom);
                m_sized = true;
            }
            const uint32_t sequence = m_shared.u32(kFieldFrameSeq);
            // The guest makes the sequence odd while it repaints (seqlock).
            // Copying during that window is what produced the white flashes;
            // an odd value means "come back next tick".
            if ((sequence & 1u) == 0 && sequence != m_lastSequence) {
                std::atomic_thread_fence(std::memory_order_acquire);
                const uint32_t width = m_shared.u32(kFieldWidth);
                const uint32_t height = m_shared.u32(kFieldHeight);
                const uint32_t stride = m_shared.u32(kFieldStride);
                if (width != 0 && height != 0 && stride >= width * 4) {
                    // Snapshot the guest's finished frame instead of painting
                    // the live buffer, which the guest is writing.
                    const QImage live(m_shared.data + kPixels, static_cast<int>(width),
                                      static_cast<int>(height), static_cast<int>(stride),
                                      QImage::Format_RGBA8888);
                    if (m_frame.width() != static_cast<int>(width) ||
                        m_frame.height() != static_cast<int>(height)) {
                        m_frame = QImage(static_cast<int>(width), static_cast<int>(height),
                                         QImage::Format_RGBA8888);
                    }
                    for (uint32_t row = 0; row < height; ++row) {
                        std::memcpy(m_frame.scanLine(static_cast<int>(row)),
                                    live.constScanLine(static_cast<int>(row)),
                                    static_cast<size_t>(width) * 4);
                    }
                    std::atomic_thread_fence(std::memory_order_acquire);
                    if (m_shared.u32(kFieldFrameSeq) != sequence) {
                        // The guest started the next frame while we copied:
                        // drop this one and try again on the next tick.
                        return;
                    }
                }
                m_lastSequence = sequence;
                update();
            }
            const bool overLink = m_shared.u32(kFieldCursor) != 0;
            if (overLink != m_overLink) {
                m_overLink = overLink;
                setCursor(overLink ? Qt::PointingHandCursor : Qt::ArrowCursor);
            }
        });
        timer->start(16);
    }

    void setZoom(int percent) { applyZoom(percent); }

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
            painter.drawText(rect().adjusted(24, 96, -24, 0), Qt::AlignHCenter | Qt::AlignTop,
                             "type  wolfser  in the guest console");
            return;
        }
        // The guest publishes frame_seq after the pixels; pair it with an
        // acquire fence so the QImage below sees the completed frame.
        std::atomic_thread_fence(std::memory_order_acquire);
        if (m_frame.isNull()) {
            return;
        }
        QPainter painter(this);
        painter.setRenderHint(QPainter::SmoothPixmapTransform, m_zoom > 100);
        painter.drawImage(rect(), m_frame);
    }

    void keyPressEvent(QKeyEvent *event) override {
        // Qt repeats a held key; the input ring must receive one press only.
        if (event->isAutoRepeat()) {
            return;
        }
        if (event->modifiers() & Qt::ControlModifier) {
            switch (event->key()) {
            case Qt::Key_Plus:
            case Qt::Key_Equal:
                applyZoom(m_zoom + 10);
                return;
            case Qt::Key_Minus:
                applyZoom(m_zoom - 10);
                return;
            case Qt::Key_0:
                applyZoom(100);
                return;
            default:
                break;
            }
        }
        uint16_t key = 0;
        switch (event->key()) {
        case Qt::Key_Up: key = 1; break;
        case Qt::Key_Down: key = 2; break;
        case Qt::Key_PageUp: key = 3; break;
        case Qt::Key_PageDown: key = 4; break;
        case Qt::Key_Home:
            if (event->modifiers() & Qt::AltModifier) {
                key = 22; // start page
                break;
            }
            key = 5;
            break;
        case Qt::Key_End: key = 6; break;
        case Qt::Key_Return:
        case Qt::Key_Enter: key = 10; break;
        case Qt::Key_Backspace: key = 11; break;
        case Qt::Key_Escape: key = 12; break;
        case Qt::Key_Left:
            if (event->modifiers() & Qt::AltModifier) {
                key = 14; // back
                break;
            }
            return;
        case Qt::Key_Right:
            if (event->modifiers() & Qt::AltModifier) {
                key = 15; // forward
                break;
            }
            return;
        case Qt::Key_F5:
            key = 16; // reload
            break;
        case Qt::Key_Tab:
            if (event->modifiers() & Qt::ControlModifier) {
                key = 20; // next tab
                break;
            }
            return;
        case Qt::Key_D:
        case Qt::Key_F:
        case Qt::Key_G:
        case Qt::Key_L:
        case Qt::Key_Q:
        case Qt::Key_R:
        case Qt::Key_T:
        case Qt::Key_W:
            if (event->modifiers() & Qt::ControlModifier) {
                switch (event->key()) {
                case Qt::Key_D: key = 23; break;
                case Qt::Key_G: key = 24; break;
                case Qt::Key_F: key = 21; break;
                case Qt::Key_L: key = 13; break;
                case Qt::Key_Q: key = 17; break;
                case Qt::Key_R: key = 16; break;
                case Qt::Key_T: key = 18; break;
                default: key = 19; break; // Ctrl+W
                }
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
        // A second identical event within 20 ms is a duplicate, not typing.
        if (key == m_lastKey && m_keyClock.isValid() && m_keyClock.elapsed() < 20) {
            return;
        }
        m_lastKey = key;
        m_keyClock.restart();
        pushInput(m_shared.data, kInputKey, 0, key, 0, 0);
    }

    void mousePressEvent(QMouseEvent *event) override {
        uint8_t value = 0;
        if (event->button() == Qt::LeftButton) {
            value = (event->modifiers() & Qt::ControlModifier) ? 3 : 1;
        } else if (event->button() == Qt::MiddleButton) {
            value = 2;
        } else {
            return;
        }
        const QPoint point = guestPoint(event->position().toPoint());
        pushInput(m_shared.data, kInputClick, value, 0,
                  static_cast<uint16_t>(qBound(0, point.x(), 65535)),
                  static_cast<uint16_t>(qBound(0, point.y(), 65535)));
    }

    void mouseMoveEvent(QMouseEvent *event) override {
        const QPoint point = event->position().toPoint();
        if (point == m_lastHover) {
            return;
        }
        if (m_hoverClock.elapsed() < 35 && m_lastHover.x() >= 0) {
            return;
        }
        m_lastHover = point;
        m_hoverClock.restart();
        const QPoint guest = guestPoint(point);
        pushInput(m_shared.data, kInputMove, 0, 0,
                  static_cast<uint16_t>(qBound(0, guest.x(), 65535)),
                  static_cast<uint16_t>(qBound(0, guest.y(), 65535)));
    }

    void leaveEvent(QEvent *event) override {
        m_lastHover = QPoint(-1, -1);
        pushInput(m_shared.data, kInputMove, 1, 0, 0, 0);
        QWidget::leaveEvent(event);
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
    void applyZoom(int percent) {
        m_zoom = qBound(50, percent, 200);
        resize(m_frameWidth * m_zoom / 100, m_frameHeight * m_zoom / 100);
    }

    QPoint guestPoint(const QPoint &point) const {
        const int width = this->width() > 0 ? this->width() : m_frameWidth;
        const int height = this->height() > 0 ? this->height() : m_frameHeight;
        return QPoint(point.x() * m_frameWidth / width,
                      point.y() * m_frameHeight / height);
    }

    Shared &m_shared;
    uint32_t m_lastSequence = 0;
    bool m_sized = false;
    bool m_overLink = false;
    QPoint m_lastHover{-1, -1};
    QElapsedTimer m_hoverClock;
    uint16_t m_lastKey = 0;
    QElapsedTimer m_keyClock;
    QImage m_frame;
    int m_zoom = 100;
    int m_frameWidth = 900;
    int m_frameHeight = 640;
};

int snapshot(Shared &shared, const QString &path, int timeoutSeconds) {
    QElapsedTimer timer;
    timer.start();
    while (timer.elapsed() < timeoutSeconds * 1000) {
        const uint32_t sequence = shared.u32(kFieldFrameSeq);
        if (shared.u32(kFieldMagic) == kMagic &&
            sequence != 0 && (sequence & 1u) == 0 &&
            shared.u32(kFieldFormat) == kFormatRgba8888) {
            const uint32_t width = shared.u32(kFieldWidth);
            const uint32_t height = shared.u32(kFieldHeight);
            const uint32_t stride = shared.u32(kFieldStride);
            if (width == 0 || height == 0 || stride < width * 4) {
                std::fprintf(stderr, "viewer: invalid geometry\n");
                return 2;
            }
            std::atomic_thread_fence(std::memory_order_acquire);
            QImage frame(static_cast<int>(width), static_cast<int>(height),
                         QImage::Format_RGBA8888);
            const QImage live(shared.data + kPixels, static_cast<int>(width),
                              static_cast<int>(height), static_cast<int>(stride),
                              QImage::Format_RGBA8888);
            for (uint32_t row = 0; row < height; ++row) {
                std::memcpy(frame.scanLine(static_cast<int>(row)),
                            live.constScanLine(static_cast<int>(row)),
                            static_cast<size_t>(width) * 4);
            }
            std::atomic_thread_fence(std::memory_order_acquire);
            if (shared.u32(kFieldFrameSeq) != sequence) {
                QCoreApplication::processEvents();
                continue;
            }
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
    QString waitingSnapshotPath;
    int timeout = 20;
    int scalePercent = 100;
    bool headless = false;
    bool watchMode = false;

    for (int i = 1; i < argc; ++i) {
        const std::string arg = argv[i];
        if (arg == "--shm" && i + 1 < argc) {
            shmPath = QString::fromLocal8Bit(argv[++i]);
        } else if (arg == "--snapshot" && i + 1 < argc) {
            snapshotPath = QString::fromLocal8Bit(argv[++i]);
            headless = true;
        } else if (arg == "--timeout" && i + 1 < argc) {
            timeout = std::atoi(argv[++i]);
        } else if ((arg == "--scale" || arg == "--zoom") && i + 1 < argc) {
            scalePercent = std::atoi(argv[++i]);
        } else if (arg == "--watch") {
            watchMode = true;
        } else if (arg == "--snapshot-waiting" && i + 1 < argc) {
            waitingSnapshotPath = QString::fromLocal8Bit(argv[++i]);
        } else {
            std::fprintf(stderr,
                         "usage: %s [--shm PATH] [--snapshot OUT.png] [--timeout SECS] "
                         "[--scale %%] [--watch] [--snapshot-waiting OUT.png]\n",
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

    if (!waitingSnapshotPath.isEmpty()) {
        Viewer viewer(shared);
        viewer.setZoom(scalePercent);
        viewer.show();
        QCoreApplication::processEvents();
        if (!viewer.grab().save(waitingSnapshotPath)) {
            std::fprintf(stderr, "viewer: cannot write %s\n",
                         waitingSnapshotPath.toUtf8().constData());
            return 2;
        }
        std::printf("snapshot-waiting: 900x640 -> %s\n",
                    waitingSnapshotPath.toUtf8().constData());
        return 0;
    }

    if (watchMode) {
        // Start hidden with QEMU and show the window only when the guest asks
        // for it (the user typed `wolfser`). Nothing appears before that.
        auto *proxy = new WebProxy(shared, &app);
        Q_UNUSED(proxy);
        auto *viewer = new Viewer(shared);
        viewer->setZoom(scalePercent);
        auto *timer = new QTimer(&app);
        QObject::connect(timer, &QTimer::timeout, [&app, &shared, viewer, timer] {
            if (shared.u32(kFieldMagic) == kMagic &&
                shared.u32(kFieldWindowRequest) != 0) {
                std::fprintf(stderr, "viewer: Wolfser requested, showing window\n");
                viewer->show();
                viewer->raise();
                viewer->activateWindow();
                timer->stop();
            }
        });
        timer->start(100);
        return app.exec();
    }

    // No wait for the WIN1 header here: the window stays open and shows a
    // waiting screen until the guest starts Wolfser.
    auto *proxy = new WebProxy(shared, &app);
    Q_UNUSED(proxy);

    Viewer viewer(shared);
    viewer.setZoom(scalePercent);
    viewer.show();
    viewer.raise();
    viewer.activateWindow();
    return app.exec();
}
