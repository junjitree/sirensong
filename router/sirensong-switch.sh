#!/bin/sh
# Hardware slider -> sirensong on/off.
#
# Called by /etc/rc.button/switch as: sirensong.sh on|off
# Reached because switch-button.@main[0].func='sirensong', which the stock
# handler dispatches as "/etc/gl-switch.d/$func.sh $action" without needing to
# know the function itself.
#
# The screen text is ours: the stock `screen_disp_switch` only knows GL's own
# functions and falls back to "Toggle Button" for anything else, so it fires
# first with that generic label and we immediately overwrite it with a real name.
#
# Polarity follows GL's convention deliberately: the slider's `pressed` position
# means on, which on this unit is the side *away from the dimple*. That reads
# backwards if you assume the dimple marks "on" — it doesn't appear to mark
# anything, and there is no LED there (`/sys/class/leds` is empty). Matching the
# stock convention keeps the slider consistent whatever it is pointed at, and the
# screen shows the actual state, so the marking doesn't have to be guessed from.
action="$1"

show() {
	# enable=true renders the "on" state, false the "off" state.
	ubus call gl_screen set \
		"{\"method\":\"switch\",\"params\":{\"enable\":$1,\"mode\":\"sirensong\",\"sub_func\":\"\"}}" \
		>/dev/null 2>&1
}

case "$action" in
on)
	uci -q set sirensong.main.enabled='1'
	uci -q commit sirensong
	/etc/init.d/sirensong start >/dev/null 2>&1
	show true
	logger -t sirensong-switch "slider on — sirensong started"
	;;
off)
	uci -q set sirensong.main.enabled='0'
	uci -q commit sirensong
	/etc/init.d/sirensong stop >/dev/null 2>&1
	show false
	logger -t sirensong-switch "slider off — sirensong stopped"
	;;
*)
	logger -t sirensong-switch "unknown action: $action"
	;;
esac

exit 0
