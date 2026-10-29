/*
 * DeckCraft's air/OS (Haiku) host: the application, its windows and the
 * system services the Rust side reaches through the C functions at the end of
 * this file (bound in src/host.rs).
 *
 * Threads: be_app runs its message loop on the process's main thread
 * (dc_host_run), each window on its own looper thread, and the Rust interface
 * on a thread of its own. Input arrives in the window threads and is queued
 * here, tagged with its window's id, for the Rust thread, which waits on the
 * queue between frames (dc_host_wait) and hands back finished frames as
 * 32-bit BGRA pixels (dc_host_present), or puts them straight into the screen
 * itself (the windows are direct windows; src/present.rs).
 *
 * Windows: one per egui viewport (src/viewports.rs). The Rust thread opens and
 * closes them (dc_host_open_window, dc_host_close_window) and names them by
 * the id it got; every call that touches a window looks it up and locks it
 * first. Only the Rust thread closes windows, so a window it looked up can't
 * go away under it.
 */

#include <Alert.h>
#include <Application.h>
#include <SoundPlayer.h>
#include <Bitmap.h>
#include <Clipboard.h>
#include <Cursor.h>
#include <DirectWindow.h>
#include <Directory.h>
#include <Entry.h>
#include <FilePanel.h>
#include <FindDirectory.h>
#include <Font.h>
#include <FormattingConventions.h>
#include <Input.h>
#include <InterfaceDefs.h>
#include <Locale.h>
#include <LocaleRoster.h>
#include <Message.h>
#include <Messenger.h>
#include <Mime.h>
#include <Node.h>
#include <Path.h>
#include <Region.h>
#include <Roster.h>
#include <Screen.h>
#include <String.h>
#include <Url.h>
#include <View.h>
#include <Window.h>

#include <dlfcn.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <algorithm>
#include <chrono>
#include <condition_variable>
#include <deque>
#include <map>
#include <mutex>
#include <string>
#include <vector>


extern "C" {

// Keep in step with `Event` in src/host.rs.
struct DcEvent {
	int32_t kind;
	// The window it happened in (dc_host_open_window's id); 0 for the
	// application's own events.
	int32_t window;
	int32_t buttons;
	int32_t clicks;
	uint32_t modifiers;
	int32_t key;
	int32_t raw_char;
	int32_t repeat;
	int32_t id;
	float x;
	float y;
	float dx;
	float dy;
	uint32_t text_len;
};

struct DcWindowInfo {
	float inner_left, inner_top, inner_right, inner_bottom;
	float outer_left, outer_top, outer_right, outer_bottom;
	// The monitor most of the window is on: its frame, edges excluded.
	float monitor_left, monitor_top, monitor_right, monitor_bottom;
	int32_t minimized;
	int32_t active;
	int32_t zoomed;
	int32_t fullscreen;
	int32_t hidden;
};

}


enum {
	DC_MOUSE_MOVED = 1,
	DC_MOUSE_DOWN = 2,
	DC_MOUSE_UP = 3,
	DC_WHEEL = 4,
	DC_KEY_DOWN = 5,
	DC_KEY_UP = 6,
	DC_RESIZED = 7,
	DC_FOCUS = 8,
	DC_CLOSE = 9,
	DC_OPEN_FILES = 10,
	DC_DROPPED_FILES = 11,
	DC_POINTER_GONE = 12,
	DC_QUIT_REQUESTED = 13,
	DC_DIALOG_ANSWER = 14,
	DC_MODIFIERS = 15,
	DC_MOVED = 16,
	DC_MINIMIZED = 17,
	DC_COLORS = 18,
	DC_IME = 19,
};

// What a DC_IME event's id says.
enum {
	DC_IME_PREEDIT = 1,
	DC_IME_COMMIT = 2,
	DC_IME_STOPPED = 3,
};

static const uint32 kDialogAnswer = 'dcDA';
static const char* kDialogField = "dc:dialog";


// #pragma mark - Event queue


struct QueuedEvent {
	DcEvent event;
	std::string text;
};

static std::mutex sQueueLock;
static std::condition_variable sQueueCondition;
static std::deque<QueuedEvent> sQueue;
static bool sWoken = false;
// The text of the event dc_host_next_event returned last.
static std::string sCurrentText;
static volatile bool sAllowQuit = false;


static DcEvent
MakeEvent(int32_t kind, int32_t window = 0)
{
	DcEvent event;
	memset(&event, 0, sizeof(event));
	event.kind = kind;
	event.window = window;
	event.modifiers = modifiers();
	return event;
}


static void
Push(const DcEvent& event, const std::string& text = std::string())
{
	{
		std::lock_guard<std::mutex> lock(sQueueLock);
		// A burst of moves only needs the last position.
		if (event.kind == DC_MOUSE_MOVED && !sQueue.empty()
			&& sQueue.back().event.kind == DC_MOUSE_MOVED
			&& sQueue.back().event.window == event.window
			&& sQueue.back().event.buttons == event.buttons) {
			sQueue.back().event = event;
		} else {
			QueuedEvent queued;
			queued.event = event;
			queued.text = text;
			sQueue.push_back(queued);
		}
	}
	sQueueCondition.notify_all();
}


// The paths of a message's refs, one per line. As documents, without the
// application's own executable: a launch through a link to it can hand it its
// own ref, and that is never a presentation.
static std::string
PathsOf(const BMessage* message, bool documents = false)
{
	app_info self;
	bool knowSelf = documents && be_app->GetAppInfo(&self) == B_OK;
	std::string paths;
	entry_ref ref;
	for (int32 i = 0; message->FindRef("refs", i, &ref) == B_OK; i++) {
		BEntry entry(&ref, true);
		entry_ref resolved;
		BPath path;
		if (entry.GetRef(&resolved) != B_OK || entry.GetPath(&path) != B_OK)
			continue;
		if (knowSelf && resolved == self.ref)
			continue;
		if (!paths.empty())
			paths += '\n';
		paths += path.Path();
	}
	return paths;
}


// #pragma mark - Input methods


// Each window's canvas has its own input method transaction (DcView::fIme*).


static int32
CountUtf8Characters(const std::string& text)
{
	int32 count = 0;
	for (size_t i = 0; i < text.size(); i++) {
		if (((uint8)text[i] & 0xc0) != 0x80)
			count++;
	}
	return count;
}


// #pragma mark - Direct connection


#ifndef B_DIRECT_DEVICE_PIXELS
#define B_DIRECT_DEVICE_PIXELS 0x00000400
#endif

// Keep in step with `RawDirect` in src/host.rs.
typedef struct {
	int32_t connected;
	int32_t bounds[4];
	uint32_t bytesPerRow;
	uint32_t rectCount;
	const int32_t* rects;
} DcDirect;

// Called with the window's id and its direct connection (src/host.rs).
typedef void (*DcDirectSink)(int32_t window, const DcDirect* direct);

static const uint32 kMaxDirectRects = 256;


// Frames go straight into the screen only when DECKCRAFT_PRESENT=direct asks for
// it (src/present.rs) and the GPU draws them.
static uint32
DirectPresentFlags()
{
	const char* present = getenv("DECKCRAFT_PRESENT");
	const char* renderer = getenv("DECKCRAFT_RENDERER");
	if (present == NULL || strcmp(present, "direct") != 0
		|| (renderer != NULL && strcmp(renderer, "cpu") == 0))
		return 0;
	return B_DIRECT_DEVICE_PIXELS;
}


// #pragma mark - Monitors


// The monitors (their frames in screen coordinates, logical pixels). On
// air/OS one desktop spans every monitor (BScreen's frame is all of them);
// its display layout knows each one: the enabled, connected ones that don't
// mirror another. Elsewhere, the screen. DECKCRAFT_MONITORS
// ("left,top,right,bottom;..."), for testing, splits the screen into
// monitors that aren't there.
static bool
FakeMonitors(std::vector<BRect>& monitors)
{
	const char* fake = getenv("DECKCRAFT_MONITORS");
	if (fake == NULL || fake[0] == '\0')
		return false;
	BString list(fake);
	int32 start = 0;
	while (start < list.Length()) {
		int32 end = list.FindFirst(';', start);
		if (end < 0)
			end = list.Length();
		BString item;
		list.CopyInto(item, start, end - start);
		float left, top, right, bottom;
		if (sscanf(item.String(), "%f,%f,%f,%f", &left, &top, &right, &bottom)
				== 4 && right > left && bottom > top)
			monitors.push_back(BRect(left, top, right, bottom));
		start = end + 1;
	}
	return !monitors.empty();
}


static std::vector<BRect>
Monitors()
{
	std::vector<BRect> monitors;
	if (FakeMonitors(monitors))
		return monitors;
	typedef status_t (*GetLayout)(BMessage&);
	static GetLayout getLayout = (GetLayout)dlsym(RTLD_DEFAULT,
		"_ZN8BPrivate18get_display_layoutER8BMessage");
	BMessage layout;
	if (getLayout != NULL && getLayout(layout) == B_OK) {
		BMessage display;
		for (int32 i = 0; layout.FindMessage("display", i, &display) == B_OK;
				i++) {
			BRect frame;
			if (display.GetBool("enabled", true)
				&& display.GetBool("connected", true)
				&& display.GetInt32("mirror", -1) < 0
				&& display.FindRect("frame", &frame) == B_OK
				&& frame.IsValid())
				monitors.push_back(frame);
		}
	}
	if (monitors.empty())
		monitors.push_back(BScreen().Frame());
	return monitors;
}


// The frame of the monitor most of frame is on, or the nearest one.
static BRect
MonitorFor(BRect frame)
{
	std::vector<BRect> fake;
	if (!FakeMonitors(fake)) {
		typedef status_t (*GetDisplayFrame)(BRect, bool, BRect&);
		static GetDisplayFrame getFrame = (GetDisplayFrame)dlsym(RTLD_DEFAULT,
			"_ZN8BPrivate17get_display_frameE5BRectbRS0_");
		BRect display;
		if (getFrame != NULL && getFrame(frame, false, display) == B_OK
			&& display.IsValid())
			return display;
		return BScreen().Frame();
	}
	BRect best = fake[0];
	float bestArea = -1;
	float bestDistance = 1e30f;
	BPoint middle((frame.left + frame.right) / 2, (frame.top + frame.bottom) / 2);
	for (size_t i = 0; i < fake.size(); i++) {
		BRect common = fake[i] & frame;
		float area = common.IsValid() ? common.Width() * common.Height() : -1;
		float dx = std::max(std::max(fake[i].left - middle.x, middle.x - fake[i].right), 0.0f);
		float dy = std::max(std::max(fake[i].top - middle.y, middle.y - fake[i].bottom), 0.0f);
		float distance = dx * dx + dy * dy;
		if (area > bestArea || (area < 0 && bestArea < 0 && distance < bestDistance)) {
			best = fake[i];
			bestArea = area;
			bestDistance = distance;
		}
	}
	return best;
}


// The frame of the monitor most of the window is on.
static BRect
MonitorFrame(BWindow* window)
{
	return MonitorFor(window->Frame());
}


// #pragma mark - Canvas view


// Edges a window resize drag moves (dc_host_window_command 13); none moves
// the whole window (12).
enum {
	DC_EDGE_LEFT = 1,
	DC_EDGE_RIGHT = 2,
	DC_EDGE_TOP = 4,
	DC_EDGE_BOTTOM = 8,
};


class DcView : public BView {
public:
	DcView(BRect frame, int32 id)
		:
		BView(frame, "canvas", B_FOLLOW_ALL_SIDES,
			B_WILL_DRAW | B_FRAME_EVENTS | B_INPUT_METHOD_AWARE),
		fId(id),
		fBitmap(NULL),
		fScale(1),
		fCursor(-1),
		fDragging(false),
		fDragEdges(0)
	{
		SetViewColor(B_TRANSPARENT_COLOR);
		SetDrawingMode(B_OP_COPY);
	}

	~DcView()
	{
		delete fBitmap;
	}

	virtual void Draw(BRect updateRect)
	{
		// The frame is in device pixels (fScale per logical pixel, on an
		// air/OS desktop drawn at a higher density): app_server scales the
		// logical destination back up, so it lands one to one.
		BRect painted;
		if (fBitmap != NULL) {
			BRect logical = fBitmap->Bounds();
			logical.right = (logical.right + 1) / fScale - 1;
			logical.bottom = (logical.bottom + 1) / fScale - 1;
			painted = logical & updateRect;
			if (painted.IsValid()) {
				BRect source(painted.left * fScale, painted.top * fScale,
					(painted.right + 1) * fScale - 1,
					(painted.bottom + 1) * fScale - 1);
				source = source & fBitmap->Bounds();
				DrawBitmapAsync(fBitmap, source, painted);
			}
		}
		// Uncovered parts (the window grew and the next frame isn't there
		// yet) get the panel colour instead of garbage.
		SetHighColor(ui_color(B_PANEL_BACKGROUND_COLOR));
		BRegion rest(updateRect);
		if (painted.IsValid())
			rest.Exclude(painted);
		FillRegion(&rest);
	}

	virtual void MouseDown(BPoint where)
	{
		MakeFocus(true);
		SetMouseEventMask(B_POINTER_EVENTS, B_NO_POINTER_HISTORY);
		DcEvent event = MakeEvent(DC_MOUSE_DOWN, fId);
		event.x = where.x;
		event.y = where.y;
		BMessage* message = Window()->CurrentMessage();
		if (message != NULL) {
			message->FindInt32("buttons", &event.buttons);
			message->FindInt32("clicks", &event.clicks);
		}
		Track(message, where, event.buttons);
		Push(event);
	}

	virtual void MouseUp(BPoint where)
	{
		fDragging = false;
		DcEvent event = MakeEvent(DC_MOUSE_UP, fId);
		event.x = where.x;
		event.y = where.y;
		BMessage* message = Window()->CurrentMessage();
		if (message != NULL)
			message->FindInt32("buttons", &event.buttons);
		Track(message, where, event.buttons);
		Push(event);
	}

	virtual void MouseMoved(BPoint where, uint32 transit,
		const BMessage* dragMessage)
	{
		DcEvent event = MakeEvent(DC_MOUSE_MOVED, fId);
		event.x = where.x;
		event.y = where.y;
		BMessage* message = Window()->CurrentMessage();
		if (message != NULL)
			message->FindInt32("buttons", &event.buttons);
		Track(message, where, event.buttons);
		if (fDragging) {
			// The interface asked to move or resize the window by this
			// press (an undecorated window's own title bar or edge): the
			// pointer moves the window, and the interface sees none of it.
			if (event.buttons != 0) {
				DragTo(fScreenWhere);
				return;
			}
			fDragging = false;
		}
		if (transit == B_EXITED_VIEW && event.buttons == 0)
			event.kind = DC_POINTER_GONE;
		Push(event);
	}

	// Where the pointer is on the screen and which buttons are down, as the
	// last mouse message said.
	void Track(const BMessage* message, BPoint where, int32 buttons)
	{
		if (message == NULL
			|| message->FindPoint("screen_where", &fScreenWhere) != B_OK)
			fScreenWhere = ConvertToScreen(where);
		fButtons = buttons;
	}

	// Called with the window locked: the pointer, while its button is down,
	// moves the window (edges 0) or the window's edges (DC_EDGE_*). The
	// interface asks for it while it handles the press, after the window
	// got it: by the mouse messages since, not the pointer's state now
	// (GetMouse() then may see a quick press already up, or the pointer
	// moved on, and the window would jump by that much).
	void StartDrag(int32 edges)
	{
		if (fButtons == 0)
			return;
		fDragging = true;
		fDragEdges = edges;
		fDragStart = fScreenWhere;
		fDragFrame = Window()->Frame();
	}

	void DragTo(BPoint screen)
	{
		BPoint delta = screen - fDragStart;
		BRect frame = fDragFrame;
		if (fDragEdges == 0)
			frame.OffsetBy(delta);
		else {
			float minWidth, maxWidth, minHeight, maxHeight;
			Window()->GetSizeLimits(&minWidth, &maxWidth, &minHeight,
				&maxHeight);
			if ((fDragEdges & DC_EDGE_LEFT) != 0) {
				frame.left = std::min(frame.left + delta.x,
					frame.right - minWidth);
			}
			if ((fDragEdges & DC_EDGE_RIGHT) != 0)
				frame.right = std::max(frame.right + delta.x, frame.left + minWidth);
			if ((fDragEdges & DC_EDGE_TOP) != 0) {
				frame.top = std::min(frame.top + delta.y,
					frame.bottom - minHeight);
			}
			if ((fDragEdges & DC_EDGE_BOTTOM) != 0)
				frame.bottom = std::max(frame.bottom + delta.y, frame.top + minHeight);
		}
		Window()->MoveTo(roundf(frame.left), roundf(frame.top));
		if (fDragEdges != 0)
			Window()->ResizeTo(roundf(frame.Width()), roundf(frame.Height()));
	}

	// From the Rust thread, without the window lock: where the interface's
	// caret is for this window's input method (view coordinates, logical
	// pixels), or that nothing takes typing (known false).
	void SetImeCaret(bool known, BRect caret)
	{
		std::lock_guard<std::mutex> lock(fImeLock);
		fImeCaretKnown = known;
		fImeCaret = caret;
	}

	// From the Rust thread: ends this window's input method transaction, if
	// one is in progress (the interface interrupted the composition).
	void StopIme()
	{
		BMessenger method;
		{
			std::lock_guard<std::mutex> lock(fImeLock);
			method = fImeMethod;
			fImeMethod = BMessenger();
			fImeText.clear();
		}
		if (!method.IsValid())
			return;
		BMessage message(B_INPUT_METHOD_EVENT);
		message.AddInt32("be:opcode", B_INPUT_METHOD_STOPPED);
		method.SendMessage(&message);
	}

	// Called with the window locked: one of Haiku's BCursorIDs.
	void SetCursorId(int32 id)
	{
		static BCursor* sCursors[32] = {};
		if (id < 1 || id > 31)
			id = B_CURSOR_ID_SYSTEM_DEFAULT;
		if (id == fCursor)
			return;
		if (sCursors[id] == NULL)
			sCursors[id] = new BCursor((BCursorID)id);
		SetViewCursor(sCursors[id]);
		fCursor = id;
	}

	virtual void KeyDown(const char* bytes, int32 numBytes)
	{
		Key(DC_KEY_DOWN, Window()->CurrentMessage(), bytes, numBytes);
	}

	virtual void KeyUp(const char* bytes, int32 numBytes)
	{
		Key(DC_KEY_UP, Window()->CurrentMessage(), bytes, numBytes);
	}

	void Key(int32 kind, BMessage* message, const char* bytes, int32 numBytes)
	{
		DcEvent event = MakeEvent(kind, fId);
		if (message != NULL) {
			message->FindInt32("key", &event.key);
			message->FindInt32("raw_char", &event.raw_char);
			int32 modifiers;
			if (message->FindInt32("modifiers", &modifiers) == B_OK)
				event.modifiers = modifiers;
			int32 repeat;
			if (message->FindInt32("be:key_repeat", &repeat) == B_OK)
				event.repeat = repeat;
		}
		std::string text(bytes != NULL ? bytes : "",
			bytes != NULL && numBytes > 0 ? numBytes : 0);
		event.text_len = text.size();
		Push(event, text);
	}

	virtual void FrameResized(float width, float height)
	{
		DcEvent event = MakeEvent(DC_RESIZED, fId);
		event.x = width + 1;
		event.y = height + 1;
		Push(event);
	}

	virtual void MessageReceived(BMessage* message)
	{
		if (message->WasDropped() && message->HasRef("refs")) {
			BPoint where = ConvertFromScreen(message->DropPoint());
			DcEvent event = MakeEvent(DC_DROPPED_FILES, fId);
			event.x = where.x;
			event.y = where.y;
			std::string paths = PathsOf(message, true);
			event.text_len = paths.size();
			Push(event, paths);
			return;
		}
		switch (message->what) {
			case B_MOUSE_WHEEL_CHANGED:
			{
				// A wheel's whole steps, or a touchpad's scroll gesture
				// (air/OS): its unrounded steps, and the gesture's phase in
				// clicks (1 began, 2 changed, 3 ended). id is the device's
				// subtype (2: touchpad), repeat 1 for the precise deltas.
				DcEvent event = MakeEvent(DC_WHEEL, fId);
				message->FindInt32("be:device_subtype", &event.id);
				float dx, dy;
				int32 phase;
				if (message->FindInt32("be:wheel_phase", &phase) == B_OK
					&& message->FindFloat("be:wheel_precise_delta_x", &dx) == B_OK
					&& message->FindFloat("be:wheel_precise_delta_y", &dy) == B_OK) {
					event.dx = dx;
					event.dy = dy;
					event.clicks = phase;
					event.repeat = 1;
				} else {
					message->FindFloat("be:wheel_delta_x", &event.dx);
					message->FindFloat("be:wheel_delta_y", &event.dy);
				}
				Push(event);
				break;
			}
			case B_MODIFIERS_CHANGED:
				Push(MakeEvent(DC_MODIFIERS, fId));
				break;
			case B_INPUT_METHOD_EVENT:
				InputMethodEvent(message);
				break;
			default:
				BView::MessageReceived(message);
		}
	}

	// An input method composes text in the interface's own editor (the view
	// is B_INPUT_METHOD_AWARE): its text while composing is a pre-edit, its
	// confirmed text is typed, and its candidate window goes by the caret.
	void InputMethodEvent(BMessage* message)
	{
		int32 opcode;
		if (message->FindInt32("be:opcode", &opcode) != B_OK)
			return;
		switch (opcode) {
			case B_INPUT_METHOD_STARTED:
			{
				BMessenger method;
				if (message->FindMessenger("be:reply_to", &method) != B_OK)
					break;
				std::lock_guard<std::mutex> lock(fImeLock);
				fImeMethod = method;
				fImeText.clear();
				break;
			}
			case B_INPUT_METHOD_CHANGED:
			{
				const char* string;
				if (message->FindString("be:string", &string) != B_OK)
					break;
				bool confirmed;
				if (message->FindBool("be:confirmed", &confirmed) != B_OK)
					confirmed = false;
				// The pre-edit's selected part (the clause being converted),
				// in bytes: key to raw_char.
				DcEvent event = MakeEvent(DC_IME, fId);
				event.id = confirmed ? DC_IME_COMMIT : DC_IME_PREEDIT;
				event.key = -1;
				event.raw_char = -1;
				int32 start, end;
				if (!confirmed
					&& message->FindInt32("be:selection", 0, &start) == B_OK
					&& message->FindInt32("be:selection", 1, &end) == B_OK) {
					event.key = start;
					event.raw_char = end;
				}
				std::string text(string);
				{
					// Only in a transaction: not after the interface stopped
					// it (dc_host_ime_stop), as BTextView.
					std::lock_guard<std::mutex> lock(fImeLock);
					if (!fImeMethod.IsValid())
						break;
					fImeText = confirmed ? std::string() : text;
				}
				event.text_len = text.size();
				Push(event, text);
				break;
			}
			case B_INPUT_METHOD_STOPPED:
			{
				{
					std::lock_guard<std::mutex> lock(fImeLock);
					fImeMethod = BMessenger();
					fImeText.clear();
				}
				DcEvent event = MakeEvent(DC_IME, fId);
				event.id = DC_IME_STOPPED;
				Push(event);
				break;
			}
			case B_INPUT_METHOD_LOCATION_REQUEST:
			{
				BMessenger method;
				BRect caret;
				int32 count;
				{
					std::lock_guard<std::mutex> lock(fImeLock);
					method = fImeMethod;
					count = CountUtf8Characters(fImeText);
					caret = fImeCaret;
					if (!fImeCaretKnown) {
						// Before the interface has said: the view's corner.
						font_height height;
						GetFontHeight(&height);
						caret.Set(0, 0, 0, ceilf(height.ascent + height.descent));
					}
				}
				if (!method.IsValid())
					break;
				// A point per character, all the caret's (the interface
				// doesn't say where each one is drawn), on the screen.
				BMessage reply(B_INPUT_METHOD_EVENT);
				reply.AddInt32("be:opcode", B_INPUT_METHOD_LOCATION_REQUEST);
				BPoint where = ConvertToScreen(caret.LeftTop());
				for (int32 i = 0; i < std::max(count, (int32)1); i++) {
					reply.AddPoint("be:location_reply", where);
					reply.AddFloat("be:height_reply", caret.Height());
				}
				method.SendMessage(&reply);
				break;
			}
		}
	}

	// Called with the window locked. The pixels are device pixels, scale
	// per logical pixel.
	void Present(const uint32_t* pixels, int32 width, int32 height,
		int32 x0, int32 y0, int32 x1, int32 y1, float scale)
	{
		if (scale != fScale) {
			fScale = scale;
			delete fBitmap;
			fBitmap = NULL;
		}
		if (fBitmap == NULL || fBitmap->Bounds().IntegerWidth() + 1 != width
			|| fBitmap->Bounds().IntegerHeight() + 1 != height) {
			delete fBitmap;
			fBitmap = new BBitmap(BRect(0, 0, width - 1, height - 1), B_RGB32);
			if (fBitmap->InitCheck() != B_OK) {
				delete fBitmap;
				fBitmap = NULL;
				return;
			}
			x0 = 0;
			y0 = 0;
			x1 = width;
			y1 = height;
		}
		if (x0 < 0)
			x0 = 0;
		if (y0 < 0)
			y0 = 0;
		if (x1 > width)
			x1 = width;
		if (y1 > height)
			y1 = height;
		if (x1 <= x0 || y1 <= y0)
			return;
		uint8* bits = (uint8*)fBitmap->Bits();
		int32 bytesPerRow = fBitmap->BytesPerRow();
		for (int32 y = y0; y < y1; y++) {
			memcpy(bits + (size_t)y * bytesPerRow + (size_t)x0 * 4,
				pixels + (size_t)y * width + x0, (size_t)(x1 - x0) * 4);
		}
		Invalidate(BRect(floorf(x0 / fScale), floorf(y0 / fScale),
			ceilf(x1 / fScale) - 1, ceilf(y1 / fScale) - 1));
	}

	// Called with the window locked: the rectangles ([x0, y0, x1, y1) device
	// pixels, four values each) of a frame read back from the GPU. While the
	// window puts frames straight into the screen they are drawn at once and
	// waited for: an update drawn later could put this frame over a newer one
	// the GPU has put there in between.
	void PresentRects(const uint32_t* pixels, int32 width, int32 height,
		const int32_t* rects, int32 count, float scale, bool direct)
	{
		const int32_t all[4] = { 0, 0, width, height };
		if (scale != fScale || fBitmap == NULL
			|| fBitmap->Bounds().IntegerWidth() + 1 != width
			|| fBitmap->Bounds().IntegerHeight() + 1 != height) {
			// A new bitmap: all of it.
			fScale = scale;
			delete fBitmap;
			fBitmap = new BBitmap(BRect(0, 0, width - 1, height - 1), B_RGB32);
			if (fBitmap->InitCheck() != B_OK) {
				delete fBitmap;
				fBitmap = NULL;
				return;
			}
			rects = all;
			count = 1;
		}
		uint8* bits = (uint8*)fBitmap->Bits();
		int32 bytesPerRow = fBitmap->BytesPerRow();
		for (int32 i = 0; i < count; i++) {
			int32 x0 = std::max(rects[i * 4], (int32_t)0);
			int32 y0 = std::max(rects[i * 4 + 1], (int32_t)0);
			int32 x1 = std::min(rects[i * 4 + 2], width);
			int32 y1 = std::min(rects[i * 4 + 3], height);
			if (x1 <= x0 || y1 <= y0)
				continue;
			for (int32 y = y0; y < y1; y++) {
				memcpy(bits + (size_t)y * bytesPerRow + (size_t)x0 * 4,
					pixels + (size_t)y * width + x0, (size_t)(x1 - x0) * 4);
			}
			BRect logical(floorf(x0 / fScale), floorf(y0 / fScale),
				ceilf(x1 / fScale) - 1, ceilf(y1 / fScale) - 1);
			if (direct) {
				BRect source(logical.left * fScale, logical.top * fScale,
					(logical.right + 1) * fScale - 1,
					(logical.bottom + 1) * fScale - 1);
				DrawBitmapAsync(fBitmap, source & fBitmap->Bounds(), logical);
			} else
				Invalidate(logical);
		}
		if (direct)
			Sync();
	}

private:
	int32 fId;
	BBitmap* fBitmap;
	float fScale;
	int32 fCursor;
	bool fDragging;
	int32 fDragEdges;
	BPoint fDragStart;
	BRect fDragFrame;
	BPoint fScreenWhere;
	int32 fButtons = 0;

	// The input method transaction in progress (B_INPUT_METHOD_STARTED until
	// B_INPUT_METHOD_STOPPED): where to answer, the text being composed (an
	// answer to a location request has a point per character), and where the
	// interface's caret is (view coordinates, logical pixels; from the Rust
	// thread, dc_host_ime_caret).
	std::mutex fImeLock;
	BMessenger fImeMethod;
	std::string fImeText;
	BRect fImeCaret;
	bool fImeCaretKnown = false;
};


// #pragma mark - Window


class DcWindow : public BDirectWindow {
public:
	DcWindow(int32 id, BRect frame, const char* title, window_look look,
		window_feel feel, uint32 flags)
		:
		BDirectWindow(frame, title, look, feel,
			B_ASYNCHRONOUS_CONTROLS | DirectPresentFlags() | flags),
		fId(id),
		fDirectSink(NULL),
		fDirectReported(false),
		fFullscreen(false),
		fSavedLook(look),
		fSavedFeel(feel)
	{
		memset(&fDirect, 0, sizeof(fDirect));
		fView = new DcView(Bounds(), id);
		AddChild(fView);
		fView->MakeFocus(true);
		// The interface has its own Cut, Copy, Paste, Select All and Close
		// (with a Save Changes prompt), so these keys reach it as keys.
		RemoveShortcut('W', B_COMMAND_KEY);
		RemoveShortcut('X', B_COMMAND_KEY);
		RemoveShortcut('C', B_COMMAND_KEY);
		RemoveShortcut('V', B_COMMAND_KEY);
		RemoveShortcut('A', B_COMMAND_KEY);
	}

	virtual bool QuitRequested()
	{
		if (sAllowQuit)
			return true;
		Push(MakeEvent(DC_CLOSE, fId));
		return false;
	}

	virtual void DispatchMessage(BMessage* message, BHandler* handler)
	{
		// BWindow eats every Command+key it has no shortcut for; the
		// interface's shortcuts (Command+B for bold…) need them. Command+Q
		// still quits the Haiku way.
		if (message->what == B_KEY_DOWN) {
			int32 modifiers = 0;
			int32 rawChar = 0;
			const char* bytes = NULL;
			message->FindInt32("modifiers", &modifiers);
			message->FindInt32("raw_char", &rawChar);
			message->FindString("bytes", &bytes);
			if ((modifiers & B_COMMAND_KEY) != 0 && rawChar != 'q'
				&& rawChar != 'Q') {
				fView->Key(DC_KEY_DOWN, message, bytes,
					bytes != NULL ? strlen(bytes) : 0);
				return;
			}
		}
		BWindow::DispatchMessage(message, handler);
	}

	virtual void WindowActivated(bool active)
	{
		DcEvent event = MakeEvent(DC_FOCUS, fId);
		event.x = active ? 1 : 0;
		Push(event);
	}

	virtual void FrameMoved(BPoint where)
	{
		DcEvent event = MakeEvent(DC_MOVED, fId);
		event.x = where.x;
		event.y = where.y;
		Push(event);
	}

	virtual void Minimize(bool minimize)
	{
		BWindow::Minimize(minimize);
		DcEvent event = MakeEvent(DC_MINIMIZED, fId);
		event.x = minimize ? 1 : 0;
		Push(event);
	}

	virtual void ScreenChanged(BRect frame, color_space mode)
	{
		Push(MakeEvent(DC_COLORS, fId));
	}

	virtual void MessageReceived(BMessage* message)
	{
		switch (message->what) {
			case B_COLORS_UPDATED:
				Push(MakeEvent(DC_COLORS, fId));
				break;
			case B_INPUT_METHOD_EVENT:
				// Input methods address the focus view; what is sent to the
				// window goes there too.
				fView->InputMethodEvent(message);
				break;
			default:
				BWindow::MessageReceived(message);
		}
	}

	DcView* View() const { return fView; }

	// BDirectWindow's Zoom(BPoint, float, float) hides BWindow's Zoom().
	using BWindow::Zoom;

	// The direct daemon thread, without the window lock: app_server waits
	// here (half a second at most) before it moves, covers or hides the
	// window, and the presenter returns once the GPU has finished drawing
	// where the window was. With B_DIRECT_DEVICE_PIXELS (air/OS) the bounds
	// and clipping are frame buffer pixels.
	virtual void DirectConnected(direct_buffer_info* info)
	{
		std::lock_guard<std::mutex> lock(fDirectLock);
		bool usable = (info->buffer_state & B_DIRECT_MODE_MASK)
				!= B_DIRECT_STOP
			&& info->device_scale >= 100 && info->bits_per_pixel == 32
			&& (info->pixel_format == B_RGB32
				|| info->pixel_format == B_RGBA32)
			&& info->layout == B_BUFFER_NONINTERLEAVED
			&& info->orientation == B_BUFFER_TOP_TO_BOTTOM
			&& info->clip_list_count <= kMaxDirectRects;
		fDirect.connected = usable ? 1 : 0;
		fDirect.rects = fDirectRects;
		fDirect.rectCount = 0;
		if (usable) {
			fDirect.bounds[0] = info->window_bounds.left;
			fDirect.bounds[1] = info->window_bounds.top;
			fDirect.bounds[2] = info->window_bounds.right;
			fDirect.bounds[3] = info->window_bounds.bottom;
			fDirect.bytesPerRow = info->bytes_per_row;
			fDirect.rectCount = info->clip_list_count;
			for (uint32 i = 0; i < info->clip_list_count; i++) {
				fDirectRects[i * 4] = info->clip_list[i].left;
				fDirectRects[i * 4 + 1] = info->clip_list[i].top;
				fDirectRects[i * 4 + 2] = info->clip_list[i].right;
				fDirectRects[i * 4 + 3] = info->clip_list[i].bottom;
			}
		}
		fDirectReported = true;
		if (fDirectSink != NULL)
			fDirectSink(fId, &fDirect);
	}

	// Where the window's direct connection is reported (NULL: nowhere). The
	// last one is reported at once; unsetting waits for a report in
	// progress, as the sink is called with fDirectLock held.
	void SetDirectSink(DcDirectSink sink)
	{
		std::lock_guard<std::mutex> lock(fDirectLock);
		fDirectSink = sink;
		if (sink != NULL && fDirectReported)
			sink(fId, &fDirect);
	}

	bool DirectConnectedNow()
	{
		std::lock_guard<std::mutex> lock(fDirectLock);
		return fDirect.connected != 0;
	}

	// Called with the window locked: the window fills the monitor it is
	// on (or the given one), without its border and above the other windows
	// (Deskbar's too, as a slide show is), or goes back to where it was.
	void SetFullscreen(bool fullscreen, const BRect* onMonitor = NULL)
	{
		if (fullscreen == fFullscreen)
			return;
		fFullscreen = fullscreen;
		if (fullscreen) {
			fSavedFrame = Frame();
			fSavedLook = Look();
			fSavedFeel = Feel();
			BRect monitor = onMonitor != NULL ? *onMonitor : MonitorFrame(this);
			SetLook(B_NO_BORDER_WINDOW_LOOK);
			SetFeel(B_FLOATING_ALL_WINDOW_FEEL);
			MoveTo(monitor.LeftTop());
			ResizeTo(monitor.Width(), monitor.Height());
		} else {
			SetFeel(fSavedFeel);
			SetLook(fSavedLook);
			MoveTo(fSavedFrame.LeftTop());
			ResizeTo(fSavedFrame.Width(), fSavedFrame.Height());
		}
	}

	bool IsFullscreen() const { return fFullscreen; }

	// The look and feel the window has when it isn't full screen.
	void SetNormalLook(window_look look)
	{
		fSavedLook = look;
		if (!fFullscreen)
			SetLook(look);
	}

	void SetNormalFeel(window_feel feel)
	{
		fSavedFeel = feel;
		if (!fFullscreen)
			SetFeel(feel);
	}

	int32 Id() const { return fId; }

private:
	int32 fId;
	DcView* fView;

	// The window's last direct connection and where it is reported to (the
	// presenter, src/present.rs).
	std::mutex fDirectLock;
	DcDirectSink fDirectSink;
	DcDirect fDirect;
	bool fDirectReported;
	int32_t fDirectRects[kMaxDirectRects * 4];

	bool fFullscreen;
	BRect fSavedFrame;
	window_look fSavedLook;
	window_feel fSavedFeel;
};


// The open windows by id. Only the Rust thread adds and removes them.
static std::mutex sWindowsLock;
static std::map<int32, DcWindow*> sWindows;
static int32 sNextWindow = 1;


// The window with that id, locked, or NULL. Unlock it when done.
static DcWindow*
LockWindow(int32 id)
{
	DcWindow* window;
	{
		std::lock_guard<std::mutex> lock(sWindowsLock);
		std::map<int32, DcWindow*>::iterator found = sWindows.find(id);
		if (found == sWindows.end())
			return NULL;
		window = found->second;
	}
	return window->Lock() ? window : NULL;
}


// Puts a window placed where it was last time onto the monitors there are
// now: onto the one holding most of its frame (air/OS's get_display_frame
// picks the nearest when none holds any), shrunk to fit it, and moved in
// until its tab and borders are on it.
static void
FitOnMonitor(BWindow* window, float minWidth, float minHeight)
{
	BRect display = MonitorFrame(window);
	BRect inner = window->Frame();
	BRect outer = window->DecoratorFrame();
	float borderWidth = outer.Width() - inner.Width();
	float borderHeight = outer.Height() - inner.Height();
	float width = std::max(std::min(inner.Width(),
		display.Width() - borderWidth), minWidth - 1);
	float height = std::max(std::min(inner.Height(),
		display.Height() - borderHeight), minHeight - 1);
	if (width != inner.Width() || height != inner.Height())
		window->ResizeTo(width, height);
	outer = window->DecoratorFrame();
	float dx = 0;
	float dy = 0;
	if (outer.right > display.right)
		dx = display.right - outer.right;
	if (outer.left + dx < display.left)
		dx = display.left - outer.left;
	if (outer.bottom > display.bottom)
		dy = display.bottom - outer.bottom;
	if (outer.top + dy < display.top)
		dy = display.top - outer.top;
	window->MoveBy(dx, dy);
}


// #pragma mark - File panels


// Shows the files an Open panel offers: folders, and files with one of the
// extensions (all files when there are none).
class ExtensionFilter : public BRefFilter {
public:
	ExtensionFilter(const char* extensions)
	{
		BString list(extensions != NULL ? extensions : "");
		int32 start = 0;
		while (start <= list.Length()) {
			int32 end = list.FindFirst('\n', start);
			if (end < 0)
				end = list.Length();
			BString extension;
			list.CopyInto(extension, start, end - start);
			extension.ToLower();
			if (extension == "*") {
				fExtensions.clear();
				fAll = true;
				return;
			}
			if (extension.Length() > 0)
				fExtensions.push_back(extension);
			start = end + 1;
		}
		fAll = fExtensions.empty();
	}

	virtual bool Filter(const entry_ref* ref, BNode* node,
		struct stat_beos* stat, const char* mimeType)
	{
		if (fAll)
			return true;
		BEntry entry(ref, true);
		if (entry.IsDirectory())
			return true;
		BString name(ref->name);
		name.ToLower();
		for (size_t i = 0; i < fExtensions.size(); i++) {
			BString suffix(".");
			suffix << fExtensions[i];
			if (name.EndsWith(suffix))
				return true;
		}
		return false;
	}

private:
	std::vector<BString> fExtensions;
	bool fAll;
};


static BFilePanel* sOpenPanel = NULL;
static ExtensionFilter* sOpenFilter = NULL;
static BFilePanel* sSavePanel = NULL;
static int32 sDialogId = -1;
static bool sDialogAnswered = true;


static void
AnswerDialog(int32 id, bool picked, const std::string& path)
{
	if (id != sDialogId || sDialogAnswered)
		return;
	sDialogAnswered = true;
	DcEvent event = MakeEvent(DC_DIALOG_ANSWER);
	event.id = id;
	event.x = picked ? 1 : 0;
	event.text_len = path.size();
	Push(event, path);
}


// #pragma mark - Application


class DcApp : public BApplication {
public:
	DcApp(const char* signature, status_t* error)
		:
		BApplication(signature, error),
		fReady(false)
	{
	}

	virtual void ReadyToRun()
	{
		fReady = true;
	}

	virtual void RefsReceived(BMessage* message)
	{
		int32 dialog;
		if (message->FindInt32(kDialogField, &dialog) == B_OK) {
			std::string path = PathsOf(message);
			AnswerDialog(dialog, !path.empty(), path);
			return;
		}
		DcEvent event = MakeEvent(DC_OPEN_FILES);
		std::string paths = PathsOf(message, true);
		event.text_len = paths.size();
		if (!paths.empty())
			Push(event, paths);
	}

	virtual void ArgvReceived(int32 argc, char** argv)
	{
		// The Rust side reads its own command line. Another launch's, handed
		// over as this copy is already running (B_SINGLE_LAUNCH), opens its
		// presentations here: the arguments that aren't options, relative to
		// the folder it was started in. --show (id 1) starts the slide show
		// on them at once, as it does at start-up; other options are ignored.
		// BApplication posts this copy's own arguments to itself before
		// B_READY_TO_RUN, without a reply address, so IsSourceRemote()
		// can't tell them from another launch's.
		BMessage* message = CurrentMessage();
		if (!fReady || message == NULL || !message->IsSourceRemote())
			return;
		const char* cwd = NULL;
		message->FindString("cwd", &cwd);
		DcEvent event = MakeEvent(DC_OPEN_FILES);
		std::string paths;
		for (int32 i = 1; i < argc; i++) {
			if (strcmp(argv[i], "--control") == 0) {
				i++;
				continue;
			}
			if (strcmp(argv[i], "--show") == 0) {
				event.id = 1;
				continue;
			}
			if (argv[i][0] == '-' && argv[i][1] != '\0')
				continue;
			BPath path;
			if (argv[i][0] != '/' && cwd != NULL)
				path.SetTo(cwd, argv[i], true);
			else
				path.SetTo(argv[i], NULL, true);
			if (path.InitCheck() != B_OK)
				continue;
			if (!paths.empty())
				paths += '\n';
			paths += path.Path();
		}
		// Even without presentations: the running copy's window comes to
		// the front, as BApplication does for a launch without arguments
		// (B_SILENT_RELAUNCH).
		event.text_len = paths.size();
		Push(event, paths);
	}

	virtual bool QuitRequested()
	{
		if (sAllowQuit)
			return BApplication::QuitRequested();
		Push(MakeEvent(DC_QUIT_REQUESTED));
		return false;
	}

	virtual void MessageReceived(BMessage* message)
	{
		switch (message->what) {
			case B_SAVE_REQUESTED:
			{
				int32 dialog;
				entry_ref directory;
				const char* name;
				if (message->FindInt32(kDialogField, &dialog) != B_OK)
					break;
				if (message->FindRef("directory", &directory) != B_OK
					|| message->FindString("name", &name) != B_OK) {
					AnswerDialog(dialog, false, std::string());
					break;
				}
				BPath path(&directory);
				path.Append(name);
				AnswerDialog(dialog, path.InitCheck() == B_OK,
					path.InitCheck() == B_OK ? path.Path() : "");
				break;
			}
			case B_CANCEL:
				// Sent whenever a panel closes, after its answer if it had
				// one.
				AnswerDialog(sDialogId, false, std::string());
				break;
			case B_SILENT_RELAUNCH:
				// A launch without arguments while this copy runs (the
				// Deskbar, Tracker's icon): BApplication only activates the
				// team, which leaves a minimized window down. The window
				// comes forward as for opened presentations.
				Push(MakeEvent(DC_OPEN_FILES));
				BApplication::MessageReceived(message);
				break;
			default:
				BApplication::MessageReceived(message);
		}
	}

private:
	// B_READY_TO_RUN has come: B_ARGV_RECEIVED is another launch's.
	bool fReady;
};


// #pragma mark - Clipboard


static void*
CopyOut(const void* data, size_t length, size_t* _length)
{
	void* copy = malloc(length > 0 ? length : 1);
	if (copy == NULL)
		return NULL;
	memcpy(copy, data, length);
	*_length = length;
	return copy;
}


// #pragma mark - C interface


extern "C" {


// Creates the application: 0, 1 when a copy started from the same file is
// already running (B_SINGLE_LAUNCH) and has been handed this launch's
// arguments, or -1.
int32_t
dc_host_init(const char* signature)
{
	if (be_app != NULL)
		return 0;
	// Not deleted when it failed: the process ends.
	status_t error = B_OK;
	new DcApp(signature, &error);
	if (error == B_ALREADY_RUNNING)
		return 1;
	return error == B_OK && be_app != NULL ? 0 : -1;
}


// How dc_host_open_window opens a window (its flags).
enum {
	DC_WINDOW_UNDECORATED = 1,
	DC_WINDOW_FIXED_SIZE = 2,
	DC_WINDOW_HIDDEN = 4,
	DC_WINDOW_ZOOMED = 8,
	DC_WINDOW_NO_CLOSE = 16,
	DC_WINDOW_NO_MINIMIZE = 32,
	DC_WINDOW_NO_ZOOM = 64,
	DC_WINDOW_FLOATING = 128,
	DC_WINDOW_FULLSCREEN = 256,
};


static uint32
WindowFlags(int32_t flags)
{
	uint32 windowFlags = 0;
	if ((flags & DC_WINDOW_FIXED_SIZE) != 0)
		windowFlags |= B_NOT_RESIZABLE | B_NOT_ZOOMABLE;
	if ((flags & DC_WINDOW_NO_CLOSE) != 0)
		windowFlags |= B_NOT_CLOSABLE;
	if ((flags & DC_WINDOW_NO_MINIMIZE) != 0)
		windowFlags |= B_NOT_MINIMIZABLE;
	if ((flags & DC_WINDOW_NO_ZOOM) != 0)
		windowFlags |= B_NOT_ZOOMABLE;
	return windowFlags;
}


// Opens a window and returns its id (0 when it couldn't): its frame's
// top-left corner (border and tab included) at (x, y), centred on the screen
// when x is negative, content size width × height, its smallest and largest
// sizes (0: none) and the DC_WINDOW_* flags.
int32_t
dc_host_open_window(const char* title, float x, float y, float width,
	float height, float minWidth, float minHeight, float maxWidth,
	float maxHeight, int32_t flags)
{
	BRect frame(0, 0, width - 1, height - 1);
	bool placed = x >= 0 && y >= 0;
	if (placed)
		frame.OffsetTo(x, y);
	int32 id;
	{
		std::lock_guard<std::mutex> lock(sWindowsLock);
		id = sNextWindow++;
	}
	DcWindow* window = new DcWindow(id, frame, title,
		(flags & DC_WINDOW_UNDECORATED) != 0
			? B_NO_BORDER_WINDOW_LOOK : B_TITLED_WINDOW_LOOK,
		(flags & DC_WINDOW_FLOATING) != 0
			? B_FLOATING_ALL_WINDOW_FEEL : B_NORMAL_WINDOW_FEEL,
		WindowFlags(flags));
	minWidth = std::max(minWidth, 1.0f);
	minHeight = std::max(minHeight, 1.0f);
	window->SetSizeLimits(minWidth - 1, maxWidth > 0 ? maxWidth - 1 : 32768,
		minHeight - 1, maxHeight > 0 ? maxHeight - 1 : 32768);
	if (!placed) {
		// Centred on the monitor of the active window (on air/OS the screen
		// spans every monitor), or on the screen. A size larger than that
		// monitor (a small or dense display) shrinks to fit it, with room
		// for the tab, borders and Deskbar.
		BRect monitor = MonitorFrame(window);
		{
			std::lock_guard<std::mutex> lock(sWindowsLock);
			for (std::map<int32, DcWindow*>::iterator it = sWindows.begin();
					it != sWindows.end(); it++) {
				if (it->second->Lock()) {
					if (it->second->IsActive())
						monitor = MonitorFrame(it->second);
					it->second->Unlock();
				}
			}
		}
		float fitWidth = monitor.Width() * 0.92f;
		float fitHeight = monitor.Height() * 0.88f;
		if (frame.Width() > fitWidth || frame.Height() > fitHeight) {
			window->ResizeTo(std::max(std::min(frame.Width(), fitWidth),
					minWidth - 1),
				std::max(std::min(frame.Height(), fitHeight), minHeight - 1));
		}
		BRect outer = window->DecoratorFrame();
		window->MoveBy(roundf(monitor.left + (monitor.Width() - outer.Width()) / 2) - outer.left,
			roundf(monitor.top + (monitor.Height() - outer.Height()) / 2) - outer.top);
	} else {
		// (x, y) is where the frame's corner goes, tab and border included,
		// fitted onto the monitors there are.
		BRect outer = window->DecoratorFrame();
		window->MoveBy(frame.left - outer.left, frame.top - outer.top);
		FitOnMonitor(window, minWidth, minHeight);
	}
	{
		std::lock_guard<std::mutex> lock(sWindowsLock);
		sWindows[id] = window;
	}
	if ((flags & DC_WINDOW_ZOOMED) != 0)
		window->Zoom();
	if ((flags & DC_WINDOW_FULLSCREEN) != 0)
		window->SetFullscreen(true);
	// A window made hidden stays locked by its maker (as BWindow's
	// constructor leaves it) until it is first shown.
	if ((flags & DC_WINDOW_HIDDEN) == 0)
		window->Show();
	return id;
}


// Closes a window dc_host_open_window opened. Its direct connection's sink
// should be unset first.
void
dc_host_close_window(int32_t id)
{
	DcWindow* window;
	{
		std::lock_guard<std::mutex> lock(sWindowsLock);
		std::map<int32, DcWindow*>::iterator found = sWindows.find(id);
		if (found == sWindows.end())
			return;
		window = found->second;
		sWindows.erase(found);
	}
	if (window->Lock())
		window->Quit();
}


void
dc_host_run()
{
	be_app->Run();
	delete be_app;
}


// Called by the Rust thread once it is done: its windows close (a window it
// made hidden is still locked by it), and the application quits.
void
dc_host_quit()
{
	std::vector<int32> ids;
	{
		std::lock_guard<std::mutex> lock(sWindowsLock);
		for (std::map<int32, DcWindow*>::iterator it = sWindows.begin();
				it != sWindows.end(); it++)
			ids.push_back(it->first);
	}
	for (size_t i = 0; i < ids.size(); i++)
		dc_host_close_window(ids[i]);
	sAllowQuit = true;
	be_app->PostMessage(B_QUIT_REQUESTED);
}


// Waits until an event is queued, dc_host_wake is called or the timeout
// (microseconds; negative: none) passes. Returns how many events are queued.
int32_t
dc_host_wait(int64_t timeout)
{
	std::unique_lock<std::mutex> lock(sQueueLock);
	auto ready = [] { return !sQueue.empty() || sWoken; };
	if (timeout < 0)
		sQueueCondition.wait(lock, ready);
	else if (timeout > 0)
		sQueueCondition.wait_for(lock, std::chrono::microseconds(timeout), ready);
	sWoken = false;
	return (int32_t)sQueue.size();
}


void
dc_host_wake()
{
	{
		std::lock_guard<std::mutex> lock(sQueueLock);
		sWoken = true;
	}
	sQueueCondition.notify_all();
}


// Takes the next event, if any (1) or returns 0. Its text stays readable
// through dc_host_event_text until the next call.
int32_t
dc_host_next_event(DcEvent* event)
{
	std::lock_guard<std::mutex> lock(sQueueLock);
	if (sQueue.empty())
		return 0;
	*event = sQueue.front().event;
	sCurrentText = sQueue.front().text;
	event->text_len = sCurrentText.size();
	sQueue.pop_front();
	return 1;
}


const char*
dc_host_event_text()
{
	return sCurrentText.c_str();
}


void
dc_host_present(int32_t id, const uint32_t* pixels, int32_t width,
	int32_t height, int32_t x0, int32_t y0, int32_t x1, int32_t y1,
	float scale)
{
	if (width <= 0 || height <= 0 || scale <= 0)
		return;
	DcWindow* window = LockWindow(id);
	if (window == NULL)
		return;
	window->View()->Present(pixels, width, height, x0, y0, x1, y1, scale);
	window->Unlock();
}


// The rectangles ([x0, y0, x1, y1) device pixels, four values each) of a
// frame read back from the GPU.
void
dc_host_present_rects(int32_t id, const uint32_t* pixels, int32_t width,
	int32_t height, const int32_t* rects, int32_t count, float scale)
{
	if (width <= 0 || height <= 0 || scale <= 0 || count <= 0)
		return;
	DcWindow* window = LockWindow(id);
	if (window == NULL)
		return;
	window->View()->PresentRects(pixels, width, height, rects, count, scale,
		window->DirectConnectedNow());
	window->Unlock();
}


// Where a window's direct connection is reported (NULL: nowhere). The last
// one is reported at once; unsetting waits for a report in progress.
void
dc_host_direct_sink(int32_t id, DcDirectSink sink)
{
	DcWindow* window;
	{
		std::lock_guard<std::mutex> lock(sWindowsLock);
		std::map<int32, DcWindow*>::iterator found = sWindows.find(id);
		if (found == sWindows.end())
			return;
		window = found->second;
	}
	// Not with the window locked: the direct daemon thread reports without
	// it, and app_server waits for that while the window may wait for us.
	window->SetDirectSink(sink);
}


void
dc_host_window_info(int32_t id, DcWindowInfo* info)
{
	memset(info, 0, sizeof(*info));
	DcWindow* window = LockWindow(id);
	if (window == NULL)
		return;
	BRect inner = window->Frame();
	BRect outer = window->DecoratorFrame();
	BRect monitor = MonitorFrame(window);
	info->inner_left = inner.left;
	info->inner_top = inner.top;
	info->inner_right = inner.right + 1;
	info->inner_bottom = inner.bottom + 1;
	info->outer_left = outer.left;
	info->outer_top = outer.top;
	info->outer_right = outer.right + 1;
	info->outer_bottom = outer.bottom + 1;
	info->monitor_left = monitor.left;
	info->monitor_top = monitor.top;
	info->monitor_right = monitor.right + 1;
	info->monitor_bottom = monitor.bottom + 1;
	info->minimized = window->IsMinimized();
	info->active = window->IsActive();
	// Haiku doesn't say whether a window is zoomed.
	info->zoomed = 0;
	info->fullscreen = window->IsFullscreen();
	info->hidden = window->IsHidden();
	window->Unlock();
}


void
dc_host_set_title(int32_t id, const char* title)
{
	DcWindow* window = LockWindow(id);
	if (window == NULL)
		return;
	if (strcmp(window->Title(), title) != 0)
		window->SetTitle(title);
	window->Unlock();
}


// The window commands of src/host.rs's `WindowCommand`.
void
dc_host_window_command(int32_t id, int32_t command, float a, float b)
{
	DcWindow* window = LockWindow(id);
	if (window == NULL)
		return;
	switch (command) {
		case 1:
		{
			// Move the frame's top-left corner (decorator included) to (a, b).
			BRect inner = window->Frame();
			BRect outer = window->DecoratorFrame();
			window->MoveTo(a + inner.left - outer.left, b + inner.top - outer.top);
			break;
		}
		case 2:
			window->ResizeTo(a - 1, b - 1);
			break;
		case 3:
			window->Minimize(a != 0);
			break;
		case 4:
			// Zoom toggles: only zoom, never un-zoom on request.
			if (a != 0)
				window->Zoom();
			break;
		case 5:
			window->Activate(true);
			break;
		case 6:
		case 14:
		{
			float minWidth, maxWidth, minHeight, maxHeight;
			window->GetSizeLimits(&minWidth, &maxWidth, &minHeight, &maxHeight);
			if (command == 6)
				window->SetSizeLimits(a - 1, maxWidth, b - 1, maxHeight);
			else {
				window->SetSizeLimits(minWidth, a > 0 ? a - 1 : 32768,
					minHeight, b > 0 ? b - 1 : 32768);
			}
			break;
		}
		case 7:
			if (a != 0 && window->IsHidden())
				window->Show();
			else if (a == 0 && !window->IsHidden())
				window->Hide();
			break;
		case 8:
			window->SetNormalLook(a != 0
				? B_TITLED_WINDOW_LOOK : B_NO_BORDER_WINDOW_LOOK);
			break;
		case 9:
		{
			uint32 flags = window->Flags();
			if (a != 0)
				flags &= ~(B_NOT_RESIZABLE | B_NOT_ZOOMABLE);
			else
				flags |= B_NOT_RESIZABLE | B_NOT_ZOOMABLE;
			window->SetFlags(flags);
			break;
		}
		case 10:
			window->SetNormalFeel(a == 1
				? B_FLOATING_ALL_WINDOW_FEEL : B_NORMAL_WINDOW_FEEL);
			break;
		case 11:
			window->SetFullscreen(a != 0);
			break;
		case 16:
		{
			// Full screen on the monitor at (a, b).
			BRect monitor = MonitorFor(BRect(a, b, a, b));
			window->SetFullscreen(true, &monitor);
			break;
		}
		case 12:
			window->View()->StartDrag(0);
			break;
		case 13:
			window->View()->StartDrag((int32)a);
			break;
		case 15:
		{
			// The title bar's buttons: 1 close, 2 minimize, 4 zoom.
			int32 buttons = (int32)a;
			uint32 flags = window->Flags()
				& ~(B_NOT_CLOSABLE | B_NOT_MINIMIZABLE | B_NOT_ZOOMABLE);
			if ((buttons & 1) == 0)
				flags |= B_NOT_CLOSABLE;
			if ((buttons & 2) == 0)
				flags |= B_NOT_MINIMIZABLE;
			if ((buttons & 4) == 0)
				flags |= B_NOT_ZOOMABLE;
			window->SetFlags(flags);
			break;
		}
	}
	window->Unlock();
}


void
dc_host_set_cursor(int32_t id, int32_t cursor)
{
	DcWindow* window = LockWindow(id);
	if (window == NULL)
		return;
	window->View()->SetCursorId(cursor);
	window->Unlock();
}


uint32_t
dc_host_modifiers()
{
	return modifiers();
}


// The interface scale: the system font size against Haiku's default 12 pt.
float
dc_host_ui_scale()
{
	float scale = be_plain_font->Size() / 12.0f;
	if (scale < 1.0f)
		scale = 1.0f;
	if (scale > 4.0f)
		scale = 4.0f;
	return scale;
}


// How many frame buffer pixels a logical pixel is: air/OS draws the desktop
// at its monitors' density (the "render scale" of its display layout, in
// percent). 1 on a Haiku without the call.
float
dc_host_device_scale()
{
	typedef status_t (*GetLayout)(BMessage&);
	static GetLayout getLayout = (GetLayout)dlsym(RTLD_DEFAULT,
		"_ZN8BPrivate18get_display_layoutER8BMessage");
	if (getLayout == NULL)
		return 1;
	BMessage layout;
	if (getLayout(layout) != B_OK)
		return 1;
	int32 best = 0;
	BMessage display;
	for (int32 i = 0; layout.FindMessage("display", i, &display) == B_OK; i++) {
		int32 scale;
		if (display.GetBool("enabled", true)
			&& display.FindInt32("render scale", &scale) == B_OK && scale > best)
			best = scale;
	}
	if (best < 100 || best > 400)
		return 1;
	return best / 100.0f;
}


// The monitors' frames (logical pixels: left, top, right, bottom; at most
// max of them) into rects; how many there are.
int32_t
dc_host_monitors(float* rects, int32_t max)
{
	std::vector<BRect> monitors = Monitors();
	for (int32_t i = 0; i < max && i < (int32_t)monitors.size(); i++) {
		rects[i * 4 + 0] = monitors[i].left;
		rects[i * 4 + 1] = monitors[i].top;
		rects[i * 4 + 2] = monitors[i].right + 1;
		rects[i * 4 + 3] = monitors[i].bottom + 1;
	}
	return (int32_t)monitors.size();
}


// Whether the panel colour (Appearance preferences) is dark.
int32_t
dc_host_dark()
{
	rgb_color color = ui_color(B_PANEL_BACKGROUND_COLOR);
	return color.Brightness() < 128 ? 1 : 0;
}


// The system's formatting conventions (en_US) and the preferred languages
// (en, de…, one per line), for the locale variables the interface reads.
char*
dc_host_locale(char** languages)
{
	BFormattingConventions conventions;
	BString id;
	BLocaleRoster* roster = BLocaleRoster::Default();
	if (roster != NULL) {
		const BLocale* locale = roster->GetDefaultLocale();
		if (locale != NULL && locale->GetFormattingConventions(&conventions) == B_OK)
			id = conventions.ID();
	}
	BString list;
	BMessage preferred;
	if (roster != NULL && roster->GetPreferredLanguages(&preferred) == B_OK) {
		const char* language;
		for (int32 i = 0; preferred.FindString("language", i, &language) == B_OK; i++) {
			if (list.Length() > 0)
				list << "\n";
			list << language;
		}
	}
	*languages = strdup(list.String());
	return strdup(id.String());
}


void
dc_free(void* data)
{
	free(data);
}


// The clipboard's data of one MIME type (malloc'd; NULL when absent).
void*
dc_clipboard_get(const char* type, size_t* length)
{
	*length = 0;
	void* copy = NULL;
	if (!be_clipboard->Lock())
		return NULL;
	BMessage* clip = be_clipboard->Data();
	const void* data;
	ssize_t size;
	if (clip != NULL && clip->FindData(type, B_MIME_TYPE, &data, &size) == B_OK
		&& size >= 0)
		copy = CopyOut(data, size, length);
	be_clipboard->Unlock();
	return copy;
}




// Replaces the clipboard with text and, when html isn't NULL, its HTML.
int32_t
dc_clipboard_set(const char* text, size_t textLength, const char* html,
	size_t htmlLength)
{
	if (!be_clipboard->Lock())
		return -1;
	be_clipboard->Clear();
	BMessage* clip = be_clipboard->Data();
	status_t status = B_ERROR;
	if (clip != NULL) {
		status = clip->AddData("text/plain", B_MIME_TYPE, text, textLength);
		if (status == B_OK && html != NULL)
			status = clip->AddData("text/html", B_MIME_TYPE, html, htmlLength);
		if (status == B_OK)
			status = be_clipboard->Commit();
	}
	be_clipboard->Unlock();
	return status == B_OK ? 0 : -1;
}


// A picture another Haiku program copied as an archived BBitmap
// ("image/bitmap", as ShowImage, Screenshot, WonderBrush and airShot do, or
// "image/x-be-bitmap"), as straight RGBA (malloc'd; NULL when there's none).
// From WordCraft's host (wc_clipboard_bitmap).
void*
dc_clipboard_bitmap(int32_t* width, int32_t* height)
{
	*width = 0;
	*height = 0;
	BMessage archive;
	bool found = false;
	if (be_clipboard->Lock()) {
		BMessage* clip = be_clipboard->Data();
		found = clip != NULL
			&& (clip->FindMessage("image/bitmap", &archive) == B_OK
				|| clip->FindMessage("image/x-be-bitmap", &archive) == B_OK);
		be_clipboard->Unlock();
	}
	if (!found)
		return NULL;
	BBitmap source(&archive);
	if (source.InitCheck() != B_OK)
		return NULL;
	BRect bounds = source.Bounds();
	int32 w = bounds.IntegerWidth() + 1;
	int32 h = bounds.IntegerHeight() + 1;
	if (w <= 0 || h <= 0 || w > 16000 || h > 16000)
		return NULL;
	BBitmap converted(bounds, B_RGBA32);
	if (converted.InitCheck() != B_OK || converted.ImportBits(&source) != B_OK)
		return NULL;
	uint8* rgba = (uint8*)malloc((size_t)w * h * 4);
	if (rgba == NULL)
		return NULL;
	bool opaque = source.ColorSpace() == B_RGB32 || source.ColorSpace() == B_RGB24
		|| source.ColorSpace() == B_RGB16 || source.ColorSpace() == B_CMAP8;
	const uint8* bits = (const uint8*)converted.Bits();
	for (int32 y = 0; y < h; y++) {
		const uint8* row = bits + (size_t)y * converted.BytesPerRow();
		uint8* out = rgba + (size_t)y * w * 4;
		for (int32 x = 0; x < w; x++) {
			out[x * 4 + 0] = row[x * 4 + 2];
			out[x * 4 + 1] = row[x * 4 + 1];
			out[x * 4 + 2] = row[x * 4 + 0];
			out[x * 4 + 3] = opaque ? 255 : row[x * 4 + 3];
		}
	}
	*width = w;
	*height = h;
	return rgba;
}


// Whether the clipboard holds a picture DeckCraft can paste: an archived
// BBitmap or encoded data of one of the types (one per line).
int32_t
dc_clipboard_has_picture(const char* types)
{
	bool has = false;
	if (!be_clipboard->Lock())
		return 0;
	BMessage* clip = be_clipboard->Data();
	if (clip != NULL) {
		has = clip->HasMessage("image/bitmap")
			|| clip->HasMessage("image/x-be-bitmap");
		BString list(types);
		int32 start = 0;
		while (!has && start < list.Length()) {
			int32 end = list.FindFirst('\n', start);
			if (end < 0)
				end = list.Length();
			BString type;
			list.CopyInto(type, start, end - start);
			has = clip->HasData(type.String(), B_MIME_TYPE);
			start = end + 1;
		}
	}
	be_clipboard->Unlock();
	return has ? 1 : 0;
}


// Puts a picture on the clipboard for every kind of program: as PNG
// ("image/png") and as an archived B_RGBA32 BBitmap ("image/bitmap", which
// ShowImage, WonderBrush and other Haiku programs paste), with text when it
// isn't NULL. rgba is width × height straight RGBA.
int32_t
dc_clipboard_set_image(const char* text, size_t textLength, const void* png,
	size_t pngLength, const uint8_t* rgba, int32_t width, int32_t height)
{
	if (width <= 0 || height <= 0)
		return -1;
	BBitmap bitmap(BRect(0, 0, width - 1, height - 1), B_RGBA32);
	if (bitmap.InitCheck() != B_OK)
		return -1;
	uint8* bits = (uint8*)bitmap.Bits();
	for (int32 y = 0; y < height; y++) {
		const uint8* in = rgba + (size_t)y * width * 4;
		uint8* row = bits + (size_t)y * bitmap.BytesPerRow();
		for (int32 x = 0; x < width; x++) {
			row[x * 4 + 0] = in[x * 4 + 2];
			row[x * 4 + 1] = in[x * 4 + 1];
			row[x * 4 + 2] = in[x * 4 + 0];
			row[x * 4 + 3] = in[x * 4 + 3];
		}
	}
	BMessage archive;
	if (bitmap.Archive(&archive) != B_OK)
		return -1;
	if (!be_clipboard->Lock())
		return -1;
	be_clipboard->Clear();
	BMessage* clip = be_clipboard->Data();
	status_t status = B_ERROR;
	if (clip != NULL) {
		status = clip->AddMessage("image/bitmap", &archive);
		if (status == B_OK && text != NULL)
			status = clip->AddData("text/plain", B_MIME_TYPE, text, textLength);
		if (status == B_OK && png != NULL && pngLength > 0)
			status = clip->AddData("image/png", B_MIME_TYPE, png, pngLength);
		if (status == B_OK)
			status = be_clipboard->Commit();
	}
	be_clipboard->Unlock();
	return status == B_OK ? 0 : -1;
}




// Shows a file panel: Open (save == 0) offering the extensions (one per line;
// "*": any file), or Save suggesting the name. Its answer arrives as a
// DC_DIALOG_ANSWER event with the id.
int32_t
dc_host_file_panel(int32_t id, int32_t save, const char* name,
	const char* extensions, const char* title)
{
	BMessenger target(be_app);
	if (save) {
		if (sSavePanel == NULL) {
			sSavePanel = new BFilePanel(B_SAVE_PANEL, &target, NULL, 0, false,
				NULL, NULL, false, true);
		}
		BMessage message(B_SAVE_REQUESTED);
		message.AddInt32(kDialogField, id);
		sSavePanel->SetMessage(&message);
		sSavePanel->SetSaveText(name != NULL ? name : "");
		if (sSavePanel->Window()->Lock()) {
			sSavePanel->Window()->SetTitle(title);
			sSavePanel->Window()->Unlock();
		}
		sDialogId = id;
		sDialogAnswered = false;
		sSavePanel->Show();
		return 0;
	}
	if (sOpenPanel == NULL) {
		sOpenPanel = new BFilePanel(B_OPEN_PANEL, &target, NULL, B_FILE_NODE,
			false, NULL, NULL, false, true);
	}
	ExtensionFilter* filter = new ExtensionFilter(extensions);
	sOpenPanel->SetRefFilter(filter);
	delete sOpenFilter;
	sOpenFilter = filter;
	BMessage message(B_REFS_RECEIVED);
	message.AddInt32(kDialogField, id);
	sOpenPanel->SetMessage(&message);
	if (sOpenPanel->Window()->Lock()) {
		sOpenPanel->Window()->SetTitle(title);
		sOpenPanel->Window()->Unlock();
	}
	sOpenPanel->Refresh();
	sDialogId = id;
	sDialogAnswered = false;
	sOpenPanel->Show();
	return 0;
}


// The window with that id without locking it (its view's input method state
// has a lock of its own), or NULL.
static DcWindow*
FindWindow(int32 id)
{
	std::lock_guard<std::mutex> lock(sWindowsLock);
	std::map<int32, DcWindow*>::iterator found = sWindows.find(id);
	return found != sWindows.end() ? found->second : NULL;
}


// Where the interface's caret is in a window, for an input method's
// candidate window (view coordinates, logical pixels), or that there's no
// text to type into there (known 0).
void
dc_host_ime_caret(int32_t id, int32_t known, float left, float top,
	float right, float bottom)
{
	DcWindow* window = FindWindow(id);
	if (window != NULL)
		window->View()->SetImeCaret(known != 0, BRect(left, top, right, bottom));
}


// Ends a window's input method transaction, if one is in progress: the
// interface interrupted the composition (the caret moved away, the editor
// lost the keyboard).
void
dc_host_ime_stop(int32_t id)
{
	DcWindow* window = FindWindow(id);
	if (window != NULL)
		window->View()->StopIme();
}


int32_t
dc_host_open_url(const char* url)
{
	BUrl target(url, true);
	if (!target.IsValid())
		return -1;
	return target.OpenWithPreferredApplication(false) == B_OK ? 0 : -1;
}


// Shows a path in Tracker: a folder in a window of its own, a file selected in
// its folder's window (as Tracker's own "Open parent" does). Tracker is
// started when it doesn't run. A path as such: BUrl takes no bare path, and a
// file:// link of one with a space or a '#' is invalid or another path.
int32_t
dc_host_reveal(const char* path)
{
	BEntry entry(path, true);
	entry_ref ref;
	if (!entry.Exists() || entry.GetRef(&ref) != B_OK)
		return -1;
	BMessage message(B_REFS_RECEIVED);
	if (entry.IsDirectory()) {
		message.AddRef("refs", &ref);
	} else {
		BEntry parent;
		entry_ref parentRef;
		node_ref node;
		if (entry.GetParent(&parent) != B_OK
			|| parent.GetRef(&parentRef) != B_OK
			|| entry.GetNodeRef(&node) != B_OK)
			return -1;
		message.AddRef("refs", &parentRef);
		message.AddData("nodeRefToSelect", B_RAW_TYPE, &node, sizeof(node));
	}
	static const char* kTracker = "application/x-vnd.Be-TRAK";
	BMessenger tracker(kTracker);
	if (tracker.IsValid())
		return tracker.SendMessage(&message) == B_OK ? 0 : -1;
	return be_roster->Launch(kTracker, &message) == B_OK ? 0 : -1;
}




// Makes the application the preferred one for the types (one per line) that
// have none yet, so Tracker opens presentations with it out of the box; a
// preferred application the user chose stays.
int32_t
dc_host_claim_types(const char* signature, const char* types)
{
	BString list(types);
	int32 claimed = 0;
	int32 start = 0;
	while (start < list.Length()) {
		int32 end = list.FindFirst('\n', start);
		if (end < 0)
			end = list.Length();
		BString type;
		list.CopyInto(type, start, end - start);
		start = end + 1;
		BMimeType mime(type.String());
		if (mime.InitCheck() != B_OK)
			continue;
		char preferred[B_MIME_TYPE_LENGTH];
		if (mime.IsInstalled() && mime.GetPreferredApp(preferred) == B_OK
			&& preferred[0] != '\0')
			continue;
		if (!mime.IsInstalled())
			mime.Install();
		if (mime.SetPreferredApp(signature) == B_OK)
			claimed++;
	}
	return claimed;
}


// Gives a file the system saved for us its MIME type (by extension and
// content), as files saved by Haiku programs have, so Tracker opens it with
// the right application. Leaves a type already set alone.
int32_t
dc_host_type_file(const char* path)
{
	return update_mime_info(path, false, true, B_UPDATE_MIME_INFO_NO_FORCE)
		>= B_OK ? 0 : -1;
}


void
dc_host_alert(const char* title, const char* text)
{
	BAlert* alert = new BAlert(title, text, "OK", NULL, NULL,
		B_WIDTH_AS_USUAL, B_STOP_ALERT);
	alert->Go();
}


}

// Media Kit owns the callback thread. Stop(true) joins it before Rust frees its cookie.
struct DcAudio {
    BSoundPlayer* player;
    void* cookie;
    void (*fill)(void*, float*, size_t, size_t, uint32_t);
};
static void
FillAudio(void* cookie, void* buffer, size_t bytes, const media_raw_audio_format& format)
{
    memset(buffer, 0, bytes);
    DcAudio* audio = static_cast<DcAudio*>(cookie);
    if (format.format == media_raw_audio_format::B_AUDIO_FLOAT)
        audio->fill(audio->cookie, static_cast<float*>(buffer), bytes / sizeof(float),
            format.channel_count, static_cast<uint32_t>(format.frame_rate));
}
extern "C" void*
dc_audio_start(void* cookie, void (*fill)(void*, float*, size_t, size_t, uint32_t))
{
    DcAudio* audio = new(std::nothrow) DcAudio;
    if (audio == NULL) return NULL;
    audio->cookie = cookie;
    audio->fill = fill;
    media_raw_audio_format format = media_raw_audio_format::wildcard;
    format.format = media_raw_audio_format::B_AUDIO_FLOAT;
    format.channel_count = 2;
    format.frame_rate = 48000;
    format.byte_order = B_MEDIA_HOST_ENDIAN;
    audio->player = new(std::nothrow) BSoundPlayer(&format, "DeckCraft audio", FillAudio, NULL, audio);
    if (audio->player == NULL || audio->player->InitCheck() != B_OK || audio->player->Start() != B_OK) {
        delete audio->player;
        delete audio;
        return NULL;
    }
    audio->player->SetHasData(true);
    return audio;
}
extern "C" void
dc_audio_stop(void* handle)
{
    DcAudio* audio = static_cast<DcAudio*>(handle);
    if (audio == NULL) return;
    audio->player->Stop(true);
    delete audio->player;
    delete audio;
}
