#!/usr/bin/env python3
"""Generate the synthetic corpus: one small recording per compiler rule.

Each case is written to corpus/<name>.json as plain rrweb events (decoded, one tab per `win`),
with a description of the rule it pins down. Goldens (corpus/<name>.expected.*) are produced by
`SPOILER_BLESS=1 cargo test --test corpus` and reviewed as diffs. Run this after editing cases.
"""
import json
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
CORPUS = ROOT / "corpus"
HOST = "https://demo.test"


class Rec:
    """A recording written event by event. Node ids are allocated as nodes are built."""

    def __init__(self):
        self.events = []
        self._id = 10

    def id(self):
        self._id += 1
        return self._id

    def el(self, tag, attrs=None, *kids):
        return {"type": 2, "id": self.id(), "tagName": tag, "attributes": attrs or {}, "childNodes": list(kids)}

    def text(self, value):
        return {"type": 3, "id": self.id(), "textContent": value}

    def push(self, t, kind, data, win="w1"):
        self.events.append({"type": kind, "timestamp": t, "data": data, "win": win})

    def page(self, t, body, path="/page", win="w1"):
        self.push(t, 4, {"href": HOST + path, "width": 1280, "height": 800}, win)
        head = self.el("head")
        html = self.el("html", {}, head, body)
        self.push(t, 2, {"node": {"type": 0, "id": self.id(), "childNodes": [html]}}, win)

    def nav(self, t, path, win="w1"):
        self.push(t, 4, {"href": HOST + path}, win)

    def mouse(self, t, kind, node, x=100, y=100, win="w1"):
        data = {"source": 2, "type": kind, "id": node["id"] if isinstance(node, dict) else node}
        if x is not None:
            data.update(x=x, y=y)
        self.push(t, 3, data, win)

    def down(self, t, node, **kw):
        self.mouse(t, 1, node, **kw)

    def click(self, t, node, **kw):
        self.mouse(t, 2, node, **kw)

    def press(self, t, node, **kw):
        """Mouse-down, then the click 50 ms later."""
        self.down(t, node, **kw)
        self.click(t + 50, node, **kw)

    def mutate(self, t, adds=(), removes=(), texts=(), attributes=(), win="w1"):
        self.push(t, 3, {"source": 0, "adds": list(adds), "removes": list(removes),
                         "texts": list(texts), "attributes": list(attributes)}, win)

    def add(self, t, parent, node, before=None, win="w1"):
        self.mutate(t, adds=[{"parentId": parent["id"], "nextId": before["id"] if before else None, "node": node}], win=win)

    def remove(self, t, parent, node, win="w1"):
        self.mutate(t, removes=[{"parentId": parent["id"], "id": node["id"]}], win=win)

    def set_text(self, t, node, value, win="w1"):
        self.mutate(t, texts=[{"id": node["id"], "value": value}], win=win)

    def set_attr(self, t, node, name, value, win="w1"):
        self.mutate(t, attributes=[{"id": node["id"], "attributes": {name: value}}], win=win)

    def input(self, t, node, text=None, checked=None, win="w1"):
        data = {"source": 5, "id": node["id"]}
        if text is not None:
            data["text"] = text
        if checked is not None:
            data["isChecked"] = checked
        self.push(t, 3, data, win)

    def select(self, t, node, start, end, win="w1"):
        self.push(t, 3, {"source": 14, "ranges": [{"start": node["id"], "startOffset": start, "end": node["id"], "endOffset": end}]}, win)

    def request(self, t, path, status, ms, win="w1"):
        entry = {"name": HOST + path, "initiatorType": "fetch", "timeOrigin": 0, "startTime": t,
                 "duration": ms, "responseStatus": status}
        self.push(t + ms, 6, {"plugin": "rrweb/network@1", "payload": {"requests": [entry]}}, win)

    def console_error(self, t, message, win="w1"):
        self.push(t, 6, {"plugin": "rrweb/console@1", "payload": {"level": "error", "payload": [message]}}, win)

    def custom(self, t, tag, payload=None, win="w1"):
        self.push(t, 5, {"tag": tag, "payload": payload or {}}, win)

    def mouse_move(self, t, win="w1"):
        self.push(t, 3, {"source": 1, "positions": [{"x": 1, "y": 1, "id": 1, "timeOffset": 0}]}, win)


def button(r, label, **attrs):
    return r.el("button", attrs, r.text(label))


def grid(r, rows, keyed=True):
    """A Name / Status / Notes grid. Returns (table, tbody, row nodes)."""
    th = lambda s: r.el("th", {}, r.text(s))
    cell = lambda col, s: r.el("td", {"role": "gridcell", "data-col": str(col)}, r.text(s))
    built = [r.el("tr", {"role": "row", **({"data-row-id": key} if keyed else {})}, *(cell(i, v) for i, v in enumerate(values)))
             for key, values in rows]
    tbody = r.el("tbody", {}, *built)
    table = r.el("table", {}, r.el("thead", {}, r.el("tr", {}, th("Name"), th("Status"), th("Notes"))), tbody)
    return table, tbody, built, cell


CASES = {}


def case(description):
    def register(build):
        CASES[build.__name__] = (description, build)
        return build
    return register


@case("A click on a control whose label changes: the change is its effect, timed from the click.")
def click_changes_text(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    r.press(1000, save)
    r.set_text(1200, save["childNodes"][0], "Saved")


@case("A click on inert content that changes nothing is dead.")
def dead_click_on_inert_text(r):
    note = r.el("div", {}, r.text("Just text"))
    r.page(0, r.el("body", {}, note))
    r.press(1000, note)


@case("A control whose only response is a request is unresponsive: nothing visible happened.")
def unresponsive_when_only_a_request_follows(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    r.press(1000, save)
    r.request(1100, "/api/save", 200, 300)


@case("A menu that opens on mouse-down reacted on press, before the click.")
def reaction_on_press(r):
    trigger, holder = button(r, "Status"), r.el("div")
    r.page(0, r.el("body", {}, trigger, holder))
    r.down(1000, trigger)
    r.add(1020, holder, r.el("div", {"role": "menu"}, r.text("Options")))
    r.click(1100, trigger)


@case("Three quick clicks on the same spot that do nothing are rage (and unresponsive).")
def rage_clicks(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    for t in (1000, 1300, 1600):
        r.press(t, save)


@case("Rage counts a double-click as two presses, so a burst is still rage when rrweb reports double-clicks.")
def rage_through_double_clicks(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    for t in (1000, 1300):
        r.press(t, save)
        r.press(t + 100, save)
        r.mouse(t + 160, 4, save)


@case("Two clicks followed by rrweb's double-click form one gesture: selecting text is a visible effect, not a stray dead click.")
def double_click_selects_text(r):
    words = r.text("Double click me")
    para = r.el("p", {}, words)
    r.page(0, r.el("body", {}, para))
    r.press(1000, para)
    r.press(1150, para)
    r.mouse(1160, 4, para)
    r.select(1170, words, 0, 6)


@case("Masked typing is one input action per field; only the length is known.")
def masked_typing(r):
    field = r.el("input", {"type": "password"})
    r.page(0, r.el("body", {}, field))
    for i, t in enumerate((1000, 1100, 1200, 1300)):
        r.input(t, field, text="*" * (i + 1))


@case("Each keystroke keeps the input's window open; a pause reopens it for the same action.")
def typing_keeps_its_window_open(r):
    field, list_ = r.el("input", {"type": "text"}), r.el("ul")
    r.page(0, r.el("body", {}, field, list_))
    r.input(1000, field, text="a")
    r.input(4000, field, text="ac")
    r.add(4300, list_, r.el("li", {}, r.text("Actions")))


@case("A checkbox toggled between mouse-down and click belongs to the click.")
def checkbox_between_press_and_click(r):
    box = r.el("input", {"type": "checkbox"})
    r.page(0, r.el("body", {}, r.el("label", {}, box, r.text("Remember"))))
    r.down(1000, box)
    r.input(1010, box, checked=True)
    r.click(1020, box)


@case("An input on a field mounted a moment ago is the page initializing a form, not typing.")
def programmatic_input_is_ignored(r):
    body = r.el("body")
    r.page(0, body)
    field = r.el("input", {"type": "text"})
    r.add(1000, body, field)
    r.input(1050, field, text="prefilled")


@case("Navigating A → B → A within ten seconds is thrash.")
def navigation_thrash(r):
    r.page(0, r.el("body"), path="/page")
    r.nav(2000, "/other")
    r.nav(5000, "/page")


@case("Going hidden can resolve a click (for example, opening another tab), but is not a render; away time cannot become a slow reaction.")
def idle_versus_hidden(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    r.press(1000, save)
    r.press(41_000, save)
    r.custom(42_000, "window hidden")
    r.custom(102_000, "window visible")
    r.press(103_000, save)


@case("A console error during a gesture flags it and is an action of its own.")
def console_error_after_click(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    r.press(1000, save)
    r.console_error(1300, "TypeError: cannot read properties of undefined")


@case("A failed request during a gesture flags it; it is reported when it completes.")
def failed_request_after_click(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    r.press(1000, save)
    r.request(1100, "/api/save", 500, 2400)


@case("Editing a grid cell is a cell change on that row and column.")
def grid_cell_edit(r):
    table, _, rows, _ = grid(r, [("r1", ["Alpha", "Queued", ""]), ("r2", ["Beta", "Done", ""])])
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save, table))
    r.press(1000, save)
    r.set_text(1200, rows[0]["childNodes"][1]["childNodes"][0], "Scheduled")


@case("Rows removed and re-added unchanged (a re-sort) are re-rendered, not edited.")
def grid_resort_is_a_rerender(r):
    table, tbody, rows, cell = grid(r, [("r1", ["Alpha", "a", ""]), ("r2", ["Beta", "b", ""])])
    sort = button(r, "Sort")
    r.page(0, r.el("body", {}, sort, table))
    r.press(1000, sort)
    r.mutate(1100, removes=[{"parentId": tbody["id"], "id": row["id"]} for row in rows])
    again = [r.el("tr", {"role": "row", "data-row-id": key}, *(cell(i, v) for i, v in enumerate(vals)))
             for key, vals in [("r2", ["Beta", "b", ""]), ("r1", ["Alpha", "a", ""])]]
    r.mutate(1300, adds=[{"parentId": tbody["id"], "nextId": None, "node": row} for row in again])


@case("A row that appears and one that leaves are row changes with their cells.")
def grid_rows_added_and_removed(r):
    table, tbody, rows, cell = grid(r, [("r1", ["Alpha", "Queued", ""])])
    add = button(r, "Add row")
    r.page(0, r.el("body", {}, add, table))
    r.press(1000, add)
    r.remove(1100, tbody, rows[0])
    r.add(1200, tbody, r.el("tr", {"role": "row", "data-row-id": "r9"}, cell(0, "Gamma"), cell(1, "New"), cell(2, "")))


@case("A toast that appears and leaves within the window is kept as a confirmation.")
def toast_that_came_and_went(r):
    save, holder = button(r, "Save"), r.el("div")
    r.page(0, r.el("body", {}, save, holder))
    r.press(1000, save)
    toast = r.el("div", {"role": "status"}, r.text("Saved"))
    r.add(1200, holder, toast)
    r.remove(1800, holder, toast)


@case("A dialog opening on one click and closing on another.")
def dialog_open_and_close(r):
    open_, holder = button(r, "Delete"), r.el("div")
    r.page(0, r.el("body", {}, open_, holder))
    r.press(1000, open_)
    cancel = button(r, "Cancel")
    dialog = r.el("div", {"role": "dialog", "aria-label": "Delete record?"}, cancel)
    r.add(1100, holder, dialog)
    r.press(5000, cancel)
    r.remove(5100, holder, dialog)


@case("A class change on the clicked control is a restyle; on other nodes it is ignored.")
def restyle_of_the_clicked_control(r):
    star, other = button(r, "Star"), r.el("div", {"class": "x"})
    r.page(0, r.el("body", {}, star, other))
    r.press(1000, star)
    r.set_attr(1100, star, "class", "starred")
    r.set_attr(1100, other, "class", "y")


@case("A subtree whose parent never arrives is never rendered; its churn is no effect.")
def detached_subtree_is_invisible(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    r.press(1000, save)
    orphan = r.el("div", {}, r.text("Never shown"))
    r.mutate(1100, adds=[{"parentId": 9999, "nextId": None, "node": orphan}])
    r.mutate(1200, removes=[{"parentId": 9999, "id": orphan["id"]}])


@case("Tabs have their own DOM and windows: a click in one does not end or claim the other's.")
def tabs_are_independent(r):
    a, b = button(r, "In A"), button(r, "In B")
    r.page(0, r.el("body", {}, a), win="w1")
    r.page(0, r.el("body", {}, b), win="w2")
    r.press(1000, a, win="w1")
    r.press(1100, b, win="w2")
    r.set_text(1500, a["childNodes"][0], "Done A", win="w1")


@case("Events PostHog stored twice are dropped (and counted).")
def duplicate_events_are_dropped(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    r.press(1000, save)
    r.press(1000, save)
    r.set_text(1200, save["childNodes"][0], "Saved")


@case("A full snapshot mid-window: changes so far are read off the old page first.")
def full_snapshot_during_a_gesture(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    r.press(1000, save)
    r.set_text(1100, save["childNodes"][0], "Saving…")
    r.page(1500, r.el("body", {}, button(r, "Saved")))


@case("Forty minutes away splits the trace into two visits.")
def absence_splits_visits(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    r.press(1000, save)
    r.press(2_401_000, save)


@case("Coverage reports content the trace cannot see and events it reads past.")
def coverage_of_opaque_content(r):
    r.page(0, r.el("body", {}, r.el("iframe", {"src": "https://other.test"}), r.el("canvas")))
    r.mouse_move(500)
    r.push(600, 3, {"source": 9, "id": 1, "type": 0, "commands": []})
    r.custom(700, "app-specific")


@case("A click on a node the recording never described is unresolved.")
def unresolved_click(r):
    r.page(0, r.el("body"))
    r.click(1000, 424242)


@case("Features match on the page's own controls before app chrome, structural keys up the chain.")
def feature_matching(r):
    search = button(r, "Search")
    icon = r.el("span", {"class": "icon"})
    save = r.el("div", {"data-testid": "save-bar"}, r.el("button", {}, icon))
    r.page(0, r.el("body", {}, search, save), path="/items/new")
    r.press(1000, search)
    r.press(3000, icon)


@case("A short visible failure message flags error_shown even when its request succeeded: the screen, not the HTTP status, reports the failure.")
def error_message_shown_on_screen(r):
    save, status = button(r, "Save note"), r.el("p")
    r.page(0, r.el("body", {}, save, status))
    r.press(1000, save)
    r.request(1100, "/api/notes", 200, 300)
    r.add(1450, status, r.text("Could not save note."))


@case("Moving a list item keeps its un-serialized descendants, so its nested button still resolves.")
def moved_subtree_keeps_its_children(r):
    nested = button(r, "Open details")
    item = r.el("li", {}, nested)
    old_list, new_list = r.el("ul", {}, item), r.el("ul")
    r.page(0, r.el("body", {}, old_list, new_list))
    r.mutate(1000, removes=[{"parentId": old_list["id"], "id": item["id"]}],
             adds=[{"parentId": new_list["id"], "nextId": None,
                    "node": {"type": 2, "id": item["id"], "tagName": "li",
                             "attributes": {}, "childNodes": []}}])
    r.press(2000, nested)


@case("Typing in two tabs keeps each field's input action open independently.")
def typing_is_per_tab(r):
    a, b = r.el("input", {"type": "text"}), r.el("input", {"type": "text"})
    r.page(0, r.el("body", {}, a), win="w1")
    r.page(0, r.el("body", {}, b), win="w2")
    r.input(1000, a, text="a", win="w1")
    r.input(1100, b, text="b", win="w2")
    r.input(1200, a, text="ac", win="w1")
    r.input(1300, b, text="bd", win="w2")


@case("A request reported after a second click belongs to the click at its start time.")
def late_request_keeps_earlier_click(r):
    a, b = button(r, "Save"), button(r, "Next")
    r.page(0, r.el("body", {}, a, b))
    r.press(1000, a)
    r.press(1500, b)
    r.request(1100, "/api/save", 500, 2300)


@case("Decoded document events run in timestamp order, not their stored order.")
def shuffled_document_events(r):
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save))
    r.press(1000, save)
    r.set_text(1200, save["childNodes"][0], "Saved")
    r.events[:] = r.events[2:] + r.events[:2]


@case("A double-click absorbs its first click so the first visible reaction is kept without a spurious dead or unresponsive gesture.")
def double_click_retains_first_reaction(r):
    button_, status = button(r, "Select"), r.el("p", {}, r.text("Waiting"))
    r.page(0, r.el("body", {}, button_, status))
    r.press(1000, button_)
    r.set_text(1100, status["childNodes"][0], "Selected")
    r.press(1200, button_)
    r.mouse(1370, 4, button_)


@case("Hidden-tab mutations replay but do not react or announce errors until visible.")
def hidden_changes_are_not_reactions(r):
    save, status = button(r, "Save"), r.el("p", {}, r.text("Waiting"))
    r.page(0, r.el("body", {}, save, status))
    r.press(1000, save)
    r.custom(1100, "window hidden")
    r.set_text(1900, status["childNodes"][0], "Save failed")
    r.custom(2000, "window visible")
    r.set_text(2100, status["childNodes"][0], "Ready")


@case("Screen-reader-only alerts and their text are not visible reactions or errors.")
def screen_reader_only_alerts(r):
    save, holder = button(r, "Save"), r.el("div")
    existing = r.el("div", {"role": "alert", "class": "sr-only"}, r.text("Waiting"))
    r.page(0, r.el("body", {}, save, holder, existing))
    r.press(1000, save)
    r.set_text(1200, existing["childNodes"][0], "Save failed")
    r.add(1300, holder, r.el("div", {"role": "alert", "style": "clip: rect(0, 0, 0, 0)"}, r.text("Save failed")))


@case("A grid cell whose new value is an error message flags the gesture.")
def error_in_grid_cell(r):
    table, _, rows, _ = grid(r, [("r1", ["Alpha", "Pending", ""])])
    save = button(r, "Save")
    r.page(0, r.el("body", {}, save, table))
    r.press(1000, save)
    r.set_text(1200, rows[0]["childNodes"][1]["childNodes"][0], "Save failed")


@case("Typing after a visit-length absence is a new input in a new visit.")
def typing_after_visit_gap(r):
    field = r.el("input", {"type": "text"})
    r.page(0, r.el("body", {}, field))
    r.input(1000, field, text="before")
    r.input(1_801_000, field, text="after")


@case("State effects say what changed, distinguishing expanded from collapsed.")
def state_transition_values(r):
    toggle = button(r, "Toggle", **{"aria-expanded": "false"})
    r.page(0, r.el("body", {}, toggle))
    r.press(1000, toggle)
    r.set_attr(1200, toggle, "aria-expanded", "true")
    r.press(4000, toggle)
    r.set_attr(4200, toggle, "aria-expanded", "false")


@case("An error message that appears only while hidden is not error_shown.")
def hidden_error_is_not_shown(r):
    save, status = button(r, "Save"), r.el("p", {}, r.text("Waiting"))
    r.page(0, r.el("body", {}, save, status))
    r.press(1000, save)
    r.custom(1100, "window hidden")
    r.set_text(1200, status["childNodes"][0], "Save failed")

def main():
    CORPUS.mkdir(exist_ok=True)
    for name, (description, build) in CASES.items():
        r = Rec()
        build(r)
        document = {"description": description, "app": "demo", "events": r.events}
        (CORPUS / f"{name}.json").write_text(json.dumps(document, indent=1) + "\n")
    print(f"wrote {len(CASES)} cases to {CORPUS}")


if __name__ == "__main__":
    main()
