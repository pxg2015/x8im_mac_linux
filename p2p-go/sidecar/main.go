// 邦信 · P2P 传输内核 sidecar
//
// 存在理由：Linux 发行版 webkit2gtk 编译时关闭了 WebRTC（ENABLE_WEB_RTC=OFF），
// Tauri 的 WebView 里 typeof RTCPeerConnection === 'undefined'，大文件直传在 Linux
// 端从架构上就不可能成立。Chromium/WebView2/WKWebView 都自带 WebRTC，只有 Linux
// 的 WebKitGTK 不行 —— 所以为 Linux 加一个原生传输内核，把 WebRTC 挪出 WebView。
//
// 与前端的关系（关键约束）：
//   * 信令仍由前端发（复用 8068 WS + 鉴权 ticket + 前端已实现的断线自愈）
//   * 本进程只负责 RTCPeerConnection / DataChannel / ICE
//   * 前端把 dc 抽象成"虚拟 DataChannel"，字节仍回到前端写 IndexedDB
//     → 接收端的断点续传 / autoSaveRecv / onFileOpen 一行都不用改
//
// 本地协议：一条 WebSocket（或一条 stdio 双工帧流）= 一个传输会话（一个 fid 一次传输）
//   文本帧  = 控制协议 JSON
//   二进制帧 = [1B flag][payload]，flag=0 二进制，flag=1 文本（DataChannel 上区分）
//
// 两种传输载体（同一个 session 逻辑，只换 peer 实现）：
//   -listen 127.0.0.1:0  本地 WS（调试/直连用；⚠ https 页面里浏览器会按混合内容拦掉 ws://）
//   -stdio               宿主（Tauri Rust）用管道拉起的帧流，走 Tauri IPC 转发，
//                        彻底绕开 WebKitGTK 的「https 页面禁止明文 ws://」限制
package main

import (
	"bufio"
	"encoding/binary"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/gorilla/websocket"
	"github.com/pion/webrtc/v4"
)

/* ------------------------------------------------------------------ */
/* 参数                                                                */
/* ------------------------------------------------------------------ */

var (
	listenAddr = flag.String("listen", "127.0.0.1:0", "本地监听地址（0=随机端口，端口写 stdout 给宿主）")
	stdioMode  = flag.Bool("stdio", false, "用 stdin/stdout 双工帧流通信（宿主用管道拉起；与 -listen 二选一）")
	token      = flag.String("token", "", "本地连接令牌（宿主生成，防止同机进程蹭用）")
	portMin    = flag.Int("portmin", 0, "ICE UDP 端口下限（0=不限制；必须与安全组放行区间对齐）")
	portMax    = flag.Int("portmax", 0, "ICE UDP 端口上限")
	ifSkip     = flag.String("ifskip", "tailscale,docker,virbr,veth,br-", "排除的网卡名（逗号分隔，子串匹配）")
	stunArg    = flag.String("stun", "stun:stun.qq.com:3478,stun:stun.miwifi.com:3478,stun:stun.cloudflare.com:3478", "默认 STUN，前端可在 init 里覆盖")
	logFile    = flag.String("log", "", "日志文件（空=stderr）")
	dbg        = flag.Bool("v", false, "详细日志")
)

var (
	skipList []string
	api      *webrtc.API
	upgrader = websocket.Upgrader{
		ReadBufferSize:  256 * 1024,
		WriteBufferSize: 256 * 1024,
		// 只监听 127.0.0.1，且要求令牌 —— 不做 Origin 校验（Tauri 的 origin 是 tauri://localhost）
		CheckOrigin: func(r *http.Request) bool { return true },
	}
)

func vlog(f string, a ...any) {
	if *dbg {
		log.Printf(f, a...)
	}
}

func main() {
	flag.Parse()

	if *logFile != "" {
		f, err := os.OpenFile(*logFile, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644)
		if err == nil {
			log.SetOutput(f)
			defer f.Close()
		}
	}
	log.SetFlags(log.LstdFlags | log.Lmicroseconds)

	for _, s := range strings.Split(*ifSkip, ",") {
		if s = strings.TrimSpace(strings.ToLower(s)); s != "" {
			skipList = append(skipList, s)
		}
	}

	var err error
	if api, err = buildAPI(); err != nil {
		log.Fatalf("初始化 WebRTC 失败: %v", err)
	}

	// stdio 模式：宿主（Rust）用管道拉起本进程，双向帧流走管道，不进 TCP —— 
	// 这是 Linux 端唯一可用的本地通道（https 页面里 ws:// 会被 WebKitGTK 按混合内容拦截）。
	if *stdioMode {
		runStdio()
		return
	}

	ln, err := net.Listen("tcp", *listenAddr)
	if err != nil {
		log.Fatalf("监听 %s 失败: %v", *listenAddr, err)
	}
	port := ln.Addr().(*net.TCPAddr).Port

	mux := http.NewServeMux()
	mux.HandleFunc("/", handleConn)

	// 宿主（Rust）从 stdout 读这一行拿到端口，再下发给前端
	fmt.Printf("X8P2P_READY {\"port\":%d,\"token\":%q,\"pid\":%d}\n", port, *token, os.Getpid())
	os.Stdout.Sync()
	log.Printf("sidecar 就绪 127.0.0.1:%d  portrange=%d-%d skip=%v", port, *portMin, *portMax, skipList)

	if err = http.Serve(ln, mux); err != nil {
		log.Fatalf("HTTP 服务退出: %v", err)
	}
}

// buildAPI 组装 SettingEngine —— 这两个设置是「打洞成功率」的关键，PoC 实测有效。
func buildAPI() (*webrtc.API, error) {
	se := webrtc.SettingEngine{}

	// 🔴 端口钉进放行区间：云主机安全组通常只开一段高位端口，而内核 ip_local_port_range
	// 常常是 32768-60999。不钉的话约四成候选落在放行区间外 —— 候选发出去收不到回包，
	// 打洞必败，且现象是「看起来在打洞，就是连不上」。
	if *portMin > 0 && *portMax > *portMin {
		if err := se.SetEphemeralUDPPortRange(uint16(*portMin), uint16(*portMax)); err != nil {
			return nil, err
		}
	}

	// 🔴 排除虚拟网卡：tailscale(100.64/10) / docker / virbr 会给出「看着像直连、
	// 其实是隧道」的 host 候选，掩盖真实打洞结果，还可能选中一条慢得多的路径。
	se.SetInterfaceFilter(func(name string) bool {
		low := strings.ToLower(name)
		for _, s := range skipList {
			if strings.Contains(low, s) {
				return false
			}
		}
		return true
	})

	se.SetSCTPMaxReceiveBufferSize(16 * 1024 * 1024)
	se.SetIncludeLoopbackCandidate(false)

	return webrtc.NewAPI(webrtc.WithSettingEngine(se)), nil
}

/* ------------------------------------------------------------------ */
/* 传输载体：WS 或 stdio 帧流                                           */
/* ------------------------------------------------------------------ */

// 帧类型（两种载体统一）
const (
	opText   byte = 1 // 控制协议 JSON
	opBinary byte = 2 // [1B flag][payload]
)

// peer 是会话对底层载体的全部要求。抽出来之后 session 完全不关心对端是
// WebSocket 还是宿主管道 —— 混合内容拦截这件事只影响「怎么送」，不影响协议。
type peer interface {
	ReadFrame() (op byte, data []byte, err error)
	WriteFrame(op byte, data []byte) error
	Close() error
}

// --- WebSocket 载体（调试/局域网直连） ---

type wsPeer struct{ c *websocket.Conn }

func (w *wsPeer) ReadFrame() (byte, []byte, error) {
	mt, d, err := w.c.ReadMessage()
	if err != nil {
		return 0, nil, err
	}
	if mt == websocket.TextMessage {
		return opText, d, nil
	}
	return opBinary, d, nil
}

func (w *wsPeer) WriteFrame(op byte, d []byte) error {
	mt := websocket.BinaryMessage
	if op == opText {
		mt = websocket.TextMessage
	}
	_ = w.c.SetWriteDeadline(time.Now().Add(30 * time.Second))
	return w.c.WriteMessage(mt, d)
}

func (w *wsPeer) Close() error { return w.c.Close() }

// --- stdio 载体（Tauri IPC 桥） ---
//
// 帧格式：[4B 小端 payloadLen][1B op][payload]
// stdout 只走帧，日志全部留在 stderr（log 默认输出），宿主分别接管两条管道。
const maxStdioFrame = 8 * 1024 * 1024

type stdioPeer struct {
	r  *bufio.Reader
	w  *bufio.Writer
	mu sync.Mutex
}

func (s *stdioPeer) ReadFrame() (byte, []byte, error) {
	var h [5]byte
	if _, err := io.ReadFull(s.r, h[:]); err != nil {
		return 0, nil, err
	}
	n := binary.LittleEndian.Uint32(h[0:4])
	if n > maxStdioFrame {
		return 0, nil, fmt.Errorf("帧过大: %d", n)
	}
	buf := make([]byte, n)
	if n > 0 {
		if _, err := io.ReadFull(s.r, buf); err != nil {
			return 0, nil, err
		}
	}
	return h[4], buf, nil
}

func (s *stdioPeer) WriteFrame(op byte, d []byte) error {
	if len(d) > maxStdioFrame {
		return fmt.Errorf("帧过大: %d", len(d))
	}
	var h [5]byte
	binary.LittleEndian.PutUint32(h[0:4], uint32(len(d)))
	h[4] = op
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, err := s.w.Write(h[:]); err != nil {
		return err
	}
	if len(d) > 0 {
		if _, err := s.w.Write(d); err != nil {
			return err
		}
	}
	return s.w.Flush()
}

func (s *stdioPeer) Close() error { return nil }

func runStdio() {
	p := &stdioPeer{
		r: bufio.NewReaderSize(os.Stdin, 1<<20),
		w: bufio.NewWriterSize(os.Stdout, 1<<20),
	}
	s := &session{p: p, out: make(chan outFrame, 256), done: make(chan struct{})}
	log.Printf("sidecar 就绪（stdio）portrange=%d-%d skip=%v", *portMin, *portMax, skipList)
	// 宿主读第一帧当握手（WS 模式对应 stdout 的 X8P2P_READY 那一行）
	s.sendCtrl(ctrlOut{T: "ready"})
	go s.writeLoop()
	s.readLoop()
}

/* ------------------------------------------------------------------ */
/* 会话：一条载体连接 = 一个 PeerConnection                             */
/* ------------------------------------------------------------------ */

type outFrame struct {
	op   byte
	data []byte
}

type session struct {
	p    peer
	pc   *webrtc.PeerConnection
	dc   *webrtc.DataChannel
	out  chan outFrame
	done chan struct{}
	once sync.Once

	dcReady  atomic.Bool // DataChannel 已 open，可以收数据帧
	pendingN atomic.Int64
	closedAt atomic.Int64
}

// 控制帧：前端 → sidecar
type ctrlIn struct {
	T          string             `json:"t"`
	SDP        string             `json:"sdp,omitempty"`
	Type       string             `json:"type,omitempty"`
	Label      string             `json:"label,omitempty"`
	Cand       *webrtc.ICECandidateInit `json:"cand,omitempty"`
	ICEServers []webrtc.ICEServer `json:"iceServers,omitempty"`
	RelayOnly  bool               `json:"relayOnly,omitempty"`
}

// 控制帧：sidecar → 前端
type ctrlOut struct {
	T     string                   `json:"t"`
	SDP   string                   `json:"sdp,omitempty"`
	Type  string                   `json:"type,omitempty"`
	Label string                   `json:"label,omitempty"`
	Cand  *webrtc.ICECandidateInit `json:"cand,omitempty"`
	S     string                   `json:"s,omitempty"`
	Msg   string                   `json:"msg,omitempty"`
	N     int64                    `json:"n,omitempty"`
}

func handleConn(w http.ResponseWriter, r *http.Request) {
	if *token != "" && r.URL.Query().Get("token") != *token {
		http.Error(w, "forbidden", http.StatusForbidden)
		log.Printf("拒绝连接：令牌不符")
		return
	}
	ws, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	s := &session{p: &wsPeer{c: ws}, out: make(chan outFrame, 256), done: make(chan struct{})}
	log.Printf("会话建立 %s", r.RemoteAddr)
	go s.writeLoop()
	s.readLoop()
}

// --- 发送（统一走队列，载体不允许并发写） ---

func (s *session) sendCtrl(c ctrlOut) {
	b, _ := json.Marshal(c)
	s.push(opText, b)
}

// push 往写队列丢一帧。
//
// 🔴 不能 close(out) 收口：pion 的连接状态回调在独立 goroutine 里，
// session 关掉之后它仍可能再发一帧 —— close 之后再 send 会直接 panic，
// 把整个 sidecar（含其它会话）打挂。改用 done channel 做统一出口。
func (s *session) push(op byte, data []byte) {
	select {
	case <-s.done:
		return
	default:
	}
	select {
	case s.out <- outFrame{op, data}:
	case <-s.done:
	case <-time.After(10 * time.Second):
		s.shutdown("out_queue_stuck")
	}
}

func (s *session) writeLoop() {
	for {
		select {
		case f := <-s.out:
			if err := s.p.WriteFrame(f.op, f.data); err != nil {
				vlog("写载体失败: %v", err)
				s.shutdown("write_error")
				return
			}
		case <-s.done:
			return
		}
	}
}

// --- 收 ---

func (s *session) readLoop() {
	defer s.shutdown("peer_closed")

	if w, ok := s.p.(*wsPeer); ok {
		w.c.SetReadLimit(4 * 1024 * 1024)
	}
	for {
		op, data, err := s.p.ReadFrame()
		if err != nil {
			return
		}
		if op == opText {
			var c ctrlIn
			if e := json.Unmarshal(data, &c); e != nil {
				// 🔴 2026-11-06：绝不能静默 continue。上一版这里是直接丢弃，结果服务端把
				// TURN 的 urls 下发成字符串（pion 只认 []string），整条 init 帧解析失败 →
				// 前端等不到任何回执 → createOffer 超时 → 只报「offer_error」，线上完全
				// 看不出是格式问题。回一条 err 让前端把真实原因带到 UI / 错误上报。
				s.sendCtrl(ctrlOut{T: "err", Msg: "bad_ctrl_json: " + e.Error(), N: int64(len(data))})
				continue
			}
			s.onCtrl(c)
			continue
		}
		s.onBinary(data)
	}
}

// onBinary DataChannel 数据帧：[1B flag][payload]
//
// 背压在这里做：SCTP 发送缓冲堆积就 sleep，等于停止读本地 WS →
// 内核 socket 缓冲填满 → 浏览器 ws.bufferedAmount 上升 → 前端 pushFile 的
// HIGH_WATER 判断自然生效。不需要额外的心跳式流控。
func (s *session) onBinary(frame []byte) {
	if len(frame) < 2 || s.dc == nil {
		return
	}
	for s.dc.BufferedAmount() > 4*1024*1024 {
		if s.closedAt.Load() > 0 {
			return
		}
		time.Sleep(2 * time.Millisecond)
	}
	flag, payload := frame[0], frame[1:]
	var err error
	if flag == 1 {
		err = s.dc.SendText(string(payload))
	} else {
		err = s.dc.Send(payload)
	}
	if err != nil {
		vlog("DataChannel 发送失败: %v", err)
		s.sendCtrl(ctrlOut{T: "err", Msg: "dc_send: " + err.Error()})
	}
}

func (s *session) onCtrl(c ctrlIn) {
	switch c.T {

	case "init":
		if s.pc != nil {
			return
		}
		cfg := webrtc.Configuration{}
		if len(c.ICEServers) > 0 {
			cfg.ICEServers = c.ICEServers
		} else {
			cfg.ICEServers = defaultICEServers()
		}
		if c.RelayOnly {
			cfg.ICETransportPolicy = webrtc.ICETransportPolicyRelay
		}
		pc, err := api.NewPeerConnection(cfg)
		if err != nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "new_pc: " + err.Error()})
			return
		}
		s.pc = pc

		pc.OnICECandidate(func(cand *webrtc.ICECandidate) {
			if cand == nil {
				vlog("ICE 收集结束")
				return
			}
			init := cand.ToJSON()
			s.sendCtrl(ctrlOut{T: "ice", Cand: &init})
		})
		pc.OnConnectionStateChange(func(st webrtc.PeerConnectionState) {
			vlog("连接状态 %s", st.String())
			s.sendCtrl(ctrlOut{T: "state", S: st.String()})
		})
		pc.OnDataChannel(func(dc *webrtc.DataChannel) {
			s.bindDC(dc, true)
		})
		s.sendCtrl(ctrlOut{T: "ready"})

	case "dc": // 发起方：创建 DataChannel
		if s.pc == nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "dc_before_init"})
			return
		}
		label := c.Label
		if label == "" {
			label = "x8file"
		}
		ordered := true
		dc, err := s.pc.CreateDataChannel(label, &webrtc.DataChannelInit{Ordered: &ordered})
		if err != nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "create_dc: " + err.Error()})
			return
		}
		s.bindDC(dc, false)

	case "offer":
		if s.pc == nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "offer_before_init"})
			return
		}
		offer, err := s.pc.CreateOffer(nil)
		if err != nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "create_offer: " + err.Error()})
			return
		}
		// trickle：不等收集完就发，候选随后逐条推给前端
		if err = s.pc.SetLocalDescription(offer); err != nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "set_local_offer: " + err.Error()})
			return
		}
		s.sendCtrl(ctrlOut{T: "sdp", Type: "offer", SDP: s.pc.LocalDescription().SDP})

	case "remote":
		if s.pc == nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "remote_before_init"})
			return
		}
		typ := webrtc.NewSDPType(c.Type)
		if typ == webrtc.SDPTypeUnknown {
			s.sendCtrl(ctrlOut{T: "err", Msg: "bad_sdp_type: " + c.Type})
			return
		}
		if err := s.pc.SetRemoteDescription(webrtc.SessionDescription{Type: typ, SDP: c.SDP}); err != nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "set_remote: " + err.Error()})
			return
		}
		s.sendCtrl(ctrlOut{T: "remote-ok"})

	case "answer":
		if s.pc == nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "answer_before_init"})
			return
		}
		ans, err := s.pc.CreateAnswer(nil)
		if err != nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "create_answer: " + err.Error()})
			return
		}
		if err = s.pc.SetLocalDescription(ans); err != nil {
			s.sendCtrl(ctrlOut{T: "err", Msg: "set_local_answer: " + err.Error()})
			return
		}
		s.sendCtrl(ctrlOut{T: "sdp", Type: "answer", SDP: s.pc.LocalDescription().SDP})

	case "ice":
		if s.pc == nil || c.Cand == nil {
			return
		}
		if err := s.pc.AddICECandidate(*c.Cand); err != nil {
			vlog("AddICECandidate 失败: %v", err)
		}

	case "close":
		s.shutdown("client_close")
	}
}

// bindDC 把 DataChannel 的消息/状态桥到本地 WS
func (s *session) bindDC(dc *webrtc.DataChannel, remote bool) {
	s.dc = dc

	dc.OnOpen(func() {
		s.dcReady.Store(true)
		if remote {
			s.sendCtrl(ctrlOut{T: "dc", Label: dc.Label()})
		}
		s.sendCtrl(ctrlOut{T: "open"})
	})

	dc.OnClose(func() {
		s.dcReady.Store(false)
		s.sendCtrl(ctrlOut{T: "close"})
	})

	dc.OnMessage(func(msg webrtc.DataChannelMessage) {
		// DataChannel 的 string/binary 语义要原样保留：前端靠 e.data 是字符串还是
		// ArrayBuffer 来区分控制帧（meta/eof/ping）与字节分片。
		var frame []byte
		if msg.IsString {
			frame = make([]byte, len(msg.Data)+1)
			frame[0] = 1
			copy(frame[1:], msg.Data)
		} else {
			frame = make([]byte, len(msg.Data)+1)
			frame[0] = 0
			copy(frame[1:], msg.Data)
		}
		s.push(opBinary, frame)
	})
}

func (s *session) shutdown(reason string) {
	s.once.Do(func() {
		s.closedAt.Store(time.Now().UnixNano())
		close(s.done)
		if s.pc != nil {
			_ = s.pc.Close()
		}
		_ = s.p.Close()
		log.Printf("会话结束 (%s)", reason)
	})
}

func defaultICEServers() []webrtc.ICEServer {
	var out []webrtc.ICEServer
	var urls []string
	for _, u := range strings.Split(*stunArg, ",") {
		if u = strings.TrimSpace(u); u != "" {
			urls = append(urls, u)
		}
	}
	if len(urls) > 0 {
		out = append(out, webrtc.ICEServer{URLs: urls})
	}
	return out
}

// 供 -h 之外的场景读配置（预留）
func envInt(k string, def int) int {
	if v := os.Getenv(k); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			return n
		}
	}
	return def
}

var _ = envInt
