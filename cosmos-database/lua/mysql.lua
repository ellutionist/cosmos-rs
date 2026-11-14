-- Copyright (C) Yichun Zhang (agentzh)

local bit = require "bit"
local Pool = require "cosmos.database._pool"
-- 基于 lua-resty-mysql 0.29 移植；版权和完整 BSD 条款见 THIRD_PARTY_NOTICES.md。
local Socket = require "cosmos.network.socket"
local TlsContext = require "cosmos.network.tcp.tls.context"
local encryption = require "cosmos.encryption"
local rsa = require "cosmos.encryption.rsa"
local ed25519 = require "cosmos.encryption.ed25519"
local hex = require "cosmos.encoding.hex"
local sub = string.sub
local strbyte = string.byte
local strchar = string.char
local strfind = string.find
local format = string.format
local strrep = string.rep
local null = cosmos.null
local band = bit.band
local bxor = bit.bxor
local bor = bit.bor
local lshift = bit.lshift
local rshift = bit.rshift
local sha1 = encryption.sha1
local concat = table.concat
local setmetatable = setmetatable
local error = error
local tonumber = tonumber

local ok, new_tab = pcall(require, "table.new")
if not ok then
    new_tab = function (narr, nrec) return {} end
end

local _M = require "cosmos.database.mysql"
if rawget(_M,"_cosmos_loaded") then return _M end
_M._cosmos_loaded=true
_M._VERSION = "0.29-cosmos"
_M.null = null

local STATE_CONNECTED = 1
local STATE_COMMAND_SENT = 2

local COM_QUIT = 0x01
local COM_PING = 0x0e
local COM_QUERY = 0x03

-- 不声明 LOCAL INFILE 能力，文件上传不属于该客户端接口。
local DEFAULT_CLIENT_FLAGS = 0x7f74f
local CLIENT_SSL = 0x00000800
local CLIENT_PLUGIN_AUTH = 0x00080000
local CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA = 0x00200000
local DEFAULT_AUTH_PLUGIN = "mysql_native_password"

local SERVER_MORE_RESULTS_EXISTS = 8

local RESP_OK = "OK"
local RESP_AUTHMOREDATA = "AUTHMOREDATA"
local RESP_LOCALINFILE = "LOCALINFILE"
local RESP_EOF = "EOF"
local RESP_ERR = "ERR"
local RESP_DATA = "DATA"

local MIN_PROTOCOL_VER = 10

local LEN_NATIVE_SCRAMBLE = 20

local FULL_PACKET_SIZE = 16777215

local CHARSET_MAP = {
    _default  = 0,
    big5      = 1,
    dec8      = 3,
    cp850     = 4,
    hp8       = 6,
    koi8r     = 7,
    latin1    = 8,
    latin2    = 9,
    swe7      = 10,
    ascii     = 11,
    ujis      = 12,
    sjis      = 13,
    hebrew    = 16,
    tis620    = 18,
    euckr     = 19,
    koi8u     = 22,
    gb2312    = 24,
    greek     = 25,
    cp1250    = 26,
    gbk       = 28,
    latin5    = 30,
    armscii8  = 32,
    utf8      = 33,
    ucs2      = 35,
    cp866     = 36,
    keybcs2   = 37,
    macce     = 38,
    macroman  = 39,
    cp852     = 40,
    latin7    = 41,
    utf8mb4   = 45,
    cp1251    = 51,
    utf16     = 54,
    utf16le   = 56,
    cp1256    = 57,
    cp1257    = 59,
    utf32     = 60,
    binary    = 63,
    geostd8   = 92,
    cp932     = 95,
    eucjpms   = 97,
    gb18030   = 248
}

local mt = { __index = _M }

local converters = new_tab(0, 9)

for i = 0x01, 0x05 do

    converters[i] = tonumber
end
converters[0x00] = tonumber

converters[0x09] = tonumber
converters[0x0d] = tonumber
converters[0xf6] = tonumber

local function _get_byte2(data, i)
    local a, b = strbyte(data, i, i + 1)
    return bor(a, lshift(b, 8)), i + 2
end

local function _get_byte3(data, i)
    local a, b, c = strbyte(data, i, i + 2)
    return bor(a, lshift(b, 8), lshift(c, 16)), i + 3
end

local function _get_byte4(data, i)
    local a, b, c, d = strbyte(data, i, i + 3)
    return bor(a, lshift(b, 8), lshift(c, 16), lshift(d, 24)), i + 4
end

local function _get_byte8(data, i)
    local a, b, c, d, e, f, g, h = strbyte(data, i, i + 7)

    local lo = bor(a, lshift(b, 8), lshift(c, 16))
    local hi = bor(e, lshift(f, 8), lshift(g, 16), lshift(h, 24))
    return lo + 16777216 * d + hi * 4294967296, i + 8

end

local function _set_byte2(n)
    return strchar(band(n, 0xff), band(rshift(n, 8), 0xff))
end

local function _set_byte3(n)
    return strchar(band(n, 0xff),
                   band(rshift(n, 8), 0xff),
                   band(rshift(n, 16), 0xff))
end

local function _set_byte4(n)
    return strchar(band(n, 0xff),
                   band(rshift(n, 8), 0xff),
                   band(rshift(n, 16), 0xff),
                   band(rshift(n, 24), 0xff))
end

local function _from_cstring(data, i)
    local last = strfind(data, "\0", i, true)
    if not last then
        return nil, nil
    end

    return sub(data, i, last - 1), last + 1
end

local function _to_cstring(data)
    return data .. "\0"
end

local function _compute_sha256_token(password, scramble)
    if password == "" then return "" end
    local stage1 = encryption.sha256(password)
    local stage2 = encryption.sha256(stage1)
    local stage3 = encryption.sha256(stage2 .. scramble)
    local bytes = new_tab(32, 0)
    for i = 1, 32 do
        bytes[i] = strchar(bxor(strbyte(stage1, i), strbyte(stage3, i)))
    end
    return concat(bytes)
end

local function _compute_token(password, scramble)
    if password == "" then
        return ""
    end

    scramble = sub(scramble, 1, LEN_NATIVE_SCRAMBLE)

    local stage1 = sha1(password)
    local stage2 = sha1(stage1)
    local stage3 = sha1(scramble .. stage2)
    local n = #stage1
    local bytes = new_tab(n, 0)
    for i = 1, n do
        bytes[i] = strchar(bxor(strbyte(stage3, i), strbyte(stage1, i)))
    end

    return concat(bytes)
end

-- 每个逻辑包可以跨越多个 16 MiB 帧，序号覆盖双向认证与结果流。
local function _send_packet(self, req, size)
    if size > self._max_packet_size then return nil, "packet size too big" end
    local offset, sent = 1, 0
    repeat
        local length = math.min(size - offset + 1, FULL_PACKET_SIZE)
        self.packet_no = ((self.packet_no or 0) + 1) % 256
        local packet = _set_byte3(length) .. strchar(self.packet_no)
                       .. sub(req, offset, offset + length - 1)
        local count, err = self.sock:send(packet)
        if not count then return nil, err end
        sent = sent + count
        offset = offset + length
        if length < FULL_PACKET_SIZE then break end
    until false
    return sent
end

local function _recv_packet(self)
    local pieces, total = {}, 0
    repeat
        local header, err = self.sock:receive(4)
        if not header then return nil, nil, "failed to receive packet header: " .. err end
        local length = _get_byte3(header, 1)
        local number = strbyte(header, 4)
        if number ~= ((self.packet_no or -1) + 1) % 256 then
            return nil, nil, format("unexpected packet sequence: expected %d, received %d",
                                    ((self.packet_no or -1) + 1) % 256, number)
        end
        self.packet_no = number
        total = total + length
        if total > self._max_packet_size then return nil, nil, "packet size too big" end
        if length > 0 then
            local data
            data, err = self.sock:receive(length)
            if not data then return nil, nil, "failed to read packet content: " .. err end
            pieces[#pieces + 1] = data
        end
        if length < FULL_PACKET_SIZE then break end
    until false
    if total == 0 then return nil, nil, "empty packet" end
    local data = concat(pieces)
    local first = strbyte(data, 1)
    local typ
    if first == 0x00 then typ = RESP_OK
    elseif first == 0x01 then typ = RESP_AUTHMOREDATA
    elseif first == 0xfb then typ = RESP_LOCALINFILE
    elseif first == 0xfe and (self.state ~= STATE_COMMAND_SENT or #data < 9) then typ = RESP_EOF
    elseif first == 0xff then typ = RESP_ERR
    else typ = RESP_DATA end
    return data, typ
end

local function _from_length_coded_bin(data, pos)
    local first = strbyte(data, pos)

    if not first then
        error("truncated length-coded value")
    end

    if first >= 0 and first <= 250 then
        return first, pos + 1
    end

    if first == 251 then
        return null, pos + 1
    end

    if first == 252 then
        pos = pos + 1
        return _get_byte2(data, pos)
    end

    if first == 253 then
        pos = pos + 1
        return _get_byte3(data, pos)
    end

    if first == 254 then
        pos = pos + 1
        return _get_byte8(data, pos)
    end

    error("invalid length-coded value")
end

local function _from_length_coded_str(data, pos)
    local len
    len, pos = _from_length_coded_bin(data, pos)
    if len == null then
        return null, pos
    end
    if len > #data - pos + 1 then error("truncated length-coded string") end
    return sub(data, pos, pos + len - 1), pos + len
end

local function _parse_ok_packet(packet)
    local res = new_tab(0, 5)
    local pos

    res.affected_rows, pos = _from_length_coded_bin(packet, 2)

    res.insert_id, pos = _from_length_coded_bin(packet, pos)

    res.server_status, pos = _get_byte2(packet, pos)

    res.warning_count, pos = _get_byte2(packet, pos)

    if pos <= #packet then res.message = sub(packet, pos) end

    return res
end

local function _parse_eof_packet(packet)
    local pos = 2

    local warning_count, pos = _get_byte2(packet, pos)
    local status_flags = _get_byte2(packet, pos)

    return warning_count, status_flags
end

local function _parse_err_packet(packet)
    local errno, pos = _get_byte2(packet, 2)
    local marker = sub(packet, pos, pos)
    local sqlstate
    if marker == '#' then

        pos = pos + 1
        sqlstate = sub(packet, pos, pos + 5 - 1)
        pos = pos + 5
    end

    local message = sub(packet, pos)
    return errno, message, sqlstate
end

local function _parse_result_set_header_packet(packet)
    local field_count, pos = _from_length_coded_bin(packet, 1)

    local extra
    if pos <= #packet then extra = _from_length_coded_bin(packet, pos) end
    if type(field_count) ~= "number" or field_count < 1 or field_count > 65535 then
        error("invalid result field count")
    end

    return field_count, extra
end

local function _parse_field_packet(data)
    if #data>65536 then error("MySQL column metadata exceeds 64 KiB") end
    local col = new_tab(0, 2)
    local catalog, db, table, orig_table, orig_name, charsetnr, length
    local pos
    catalog, pos = _from_length_coded_str(data, 1)

    db, pos = _from_length_coded_str(data, pos)
    table, pos = _from_length_coded_str(data, pos)
    orig_table, pos = _from_length_coded_str(data, pos)
    col.name, pos = _from_length_coded_str(data, pos)
    if type(col.name)~="string" or #col.name>4096 then error("invalid or oversized MySQL column name") end

    orig_name, pos = _from_length_coded_str(data, pos)

    pos = pos + 1

    charsetnr, pos = _get_byte2(data, pos)

    length, pos = _get_byte4(data, pos)

    col.type = strbyte(data, pos)

    pos = pos + 1

    col.flags, pos = _get_byte2(data, pos)

    col.decimals = strbyte(data, pos)
    pos = pos + 1

    local default = sub(data, pos + 2)
    if default and default ~= "" then
        col.default = default
    end

    return col
end

local function _parse_binary_row(data,cols,compact)
    if strbyte(data,1)~=0 then error("invalid MySQL binary row header") end
    local bitmap=math.floor((#cols+9)/8);local pos=2+bitmap
    if #data<1+bitmap then error("truncated MySQL binary null bitmap") end
    local row={}
    for i,col in ipairs(cols) do
        local byte=strbyte(data,2+math.floor((i+1)/8))
        local value
        if band(byte,lshift(1,(i+1)%8))~=0 then value=null
        else
            local nextpos;value,nextpos=_M._decode_binary(data,col.type,col.flags,pos)
            if value==nil then error(nextpos) end
            pos=nextpos
        end
        row[compact and i or col.name]=value
    end
    if pos~=#data+1 then error("unexpected trailing binary row data") end
    return row
end

local function _parse_row_data_packet(data, cols, compact)
    local pos = 1
    local ncols = #cols
    local row
    if compact then
        row = new_tab(ncols, 0)
    else
        row = new_tab(0, ncols)
    end
    for i = 1, ncols do
        local value
        value, pos = _from_length_coded_str(data, pos)
        local col = cols[i]
        local typ = col.type
        local name = col.name

        if value ~= null then
            local conv = converters[typ]
            if conv then
                value = conv(value)
                if value == nil then error("invalid numeric field") end
            end
        end

        if compact then
            row[i] = value

        else
            row[name] = value
        end
    end

    if pos ~= #data + 1 then error("unexpected trailing row data") end
    return row
end

local function _recv_field_packet(self)
    local packet, typ, err = _recv_packet(self)
    if not packet then
        return nil, err
    end

    if typ == RESP_ERR then
        local errno, msg, sqlstate = _parse_err_packet(packet)
        return nil, msg, errno, sqlstate
    end

    if typ ~= RESP_DATA then
        return nil, "bad field packet type: " .. typ
    end

    return _parse_field_packet(packet)
end

local function _read_hand_shake_packet(self)
    local packet, typ, err = _recv_packet(self)
    if not packet then
        return nil, nil, err
    end

    if typ == RESP_ERR then
        local errno, msg, sqlstate = _parse_err_packet(packet)
        return nil, nil, msg, errno, sqlstate
    end

    local protocol_ver = tonumber(strbyte(packet))
    if not protocol_ver then
        return nil, nil,
            "bad handshake initialization packet: bad protocol version"
    end

    if protocol_ver < MIN_PROTOCOL_VER then
        return nil, nil, "unsupported protocol version " .. protocol_ver
                         .. ", version " .. MIN_PROTOCOL_VER
                         .. " or higher is required"
    end

    self.protocol_ver = protocol_ver

    local server_ver, pos = _from_cstring(packet, 2)
    if not server_ver then
        return nil, nil,
            "bad handshake initialization packet: bad server version"
    end

    self._server_ver = server_ver

    local thread_id, pos = _get_byte4(packet, pos)

    local scramble = sub(packet, pos, pos + 8 - 1)
    if not scramble then
        return nil, nil, "1st part of scramble not found"
    end

    pos = pos + 9

    local capabilities
    capabilities, pos = _get_byte2(packet, pos)

    self._server_lang = strbyte(packet, pos)
    pos = pos + 1

    self._server_status, pos = _get_byte2(packet, pos)

    local more_capabilities
    more_capabilities, pos = _get_byte2(packet, pos)

    self.capabilities = bor(capabilities, lshift(more_capabilities, 16))

    pos = pos + 11

    local scramble_part2 = sub(packet, pos, pos + 12 - 1)
    if not scramble_part2 then
        return nil, nil, "2nd part of scramble not found"
    end

    pos = pos + 13

    local plugin, _
    if band(self.capabilities, CLIENT_PLUGIN_AUTH) > 0 then
        plugin, _ = _from_cstring(packet, pos)
        if not plugin then

            plugin = sub(packet, pos)
        end

    else
        plugin = DEFAULT_AUTH_PLUGIN
    end

    return scramble .. scramble_part2, plugin
end

local function _lenenc_string(data)
    local n=#data
    if n<=250 then return strchar(n)..data end
    if n<=0xffff then return "\xfc".._set_byte2(n)..data end
    if n<=0xffffff then return "\xfd".._set_byte3(n)..data end
    return "\xfe".._set_byte4(n).._set_byte4(0)..data
end

local function _append_auth_length(self, data)
    local n = #data
    if n <= 250 then return strchar(n) .. data end
    if band(self.capabilities, CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA) == 0 then
        if n <= 255 then return strchar(n) .. data end
        return nil, "server does not support length-coded authentication data"
    end
    self.DEFAULT_CLIENT_FLAGS = bor(self.DEFAULT_CLIENT_FLAGS, CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA)
    if n <= 0xffff then return "\xfc" .. _set_byte2(n) .. data end
    if n <= 0xffffff then return "\xfd" .. _set_byte3(n) .. data end
    return "\xfe" .. _set_byte4(n) .. _set_byte4(0) .. data
end

local function _write_hand_shake_response(self, auth_resp, plugin)
    local append_auth, auth_err = _append_auth_length(self, auth_resp)
    if not append_auth then return auth_err end

    if self.use_ssl then
        if band(self.capabilities, CLIENT_SSL) == 0 then
            return "ssl disabled on server"
        end

        self.DEFAULT_CLIENT_FLAGS = bor(self.DEFAULT_CLIENT_FLAGS, CLIENT_SSL)
        local req = _set_byte4(self.DEFAULT_CLIENT_FLAGS)
                    .. _set_byte4(self._max_packet_size)
                    .. strchar(self.charset)
                    .. strrep("\0", 23)

        local packet_len = 4 + 4 + 1 + 23
        local bytes, err = _send_packet(self, req, packet_len)
        if not bytes then
            return "failed to send client authentication packet: " .. err
        end

        local sock = self.sock

        local ok, err = sock:starttls(self.ssl_server_name, self.ctx)
        if not ok then
            return "failed to do ssl handshake: " .. (err or "")
        end
    end

    local req = _set_byte4(self.DEFAULT_CLIENT_FLAGS)
                .. _set_byte4(self._max_packet_size)
                .. strchar(self.charset)
                .. strrep("\0", 23)
                .. _to_cstring(self.user)
                .. append_auth
    if band(self.DEFAULT_CLIENT_FLAGS, 0x08) ~= 0 then req = req .. _to_cstring(self.database) end
    if band(self.DEFAULT_CLIENT_FLAGS, CLIENT_PLUGIN_AUTH) ~= 0 then req = req .. _to_cstring(plugin) end

    local bytes, err = _send_packet(self, req, #req)
    if not bytes then
        return "failed to send client authentication packet: " .. err
    end

    return nil
end

local function _read_auth_result(self, old_auth_data, plugin)
    local packet, typ, err = _recv_packet(self)
    if not packet then
        return nil, nil, "failed to receive the result packet: " .. err
    end

    if typ == RESP_OK then
        self._server_status = _parse_ok_packet(packet).server_status
        return RESP_OK, ""
    end

    if typ == RESP_AUTHMOREDATA then
        return sub(packet, 2), ""
    end

    if typ == RESP_EOF then
        if #packet == 1 then
            return nil, "mysql_old_password"
        end

        local pos

        plugin, pos = _from_cstring(packet, 2)
        if not plugin then
            return nil, nil, "malformed packet"
        end

        local challenge = sub(packet, pos)
        -- MySQL 原生与 SHA-2 插件的 nonce 是 20 字节，认证切换包可能附带终止 NUL。
        if plugin == "mysql_native_password" or plugin == "caching_sha2_password"
           or plugin == "sha256_password" then challenge = sub(challenge, 1, 20) end
        return challenge, plugin
    end

    if typ == RESP_ERR then
        local errno, msg, sqlstate = _parse_err_packet(packet)
        return errno, sqlstate, msg
    end

    return nil, nil, "bad packet type: " .. typ
end

local function _read_ok_result(self)
    local packet, typ, err = _recv_packet(self)
    if not packet then
        return "failed to receive the result packet: " .. err
    end

    if typ == RESP_ERR then
        local errno, msg, sqlstate = _parse_err_packet(packet)
        return msg, errno, sqlstate
    end

    if typ ~= RESP_OK then
        return "bad packet type: " .. typ
    end
    self._server_status = _parse_ok_packet(packet).server_status
end

local function _encrypt_password(self, auth_data, public_key)

    local password = _to_cstring(self.password)
    local n = #password
    local l = #auth_data
    if l == 0 then return nil, "empty authentication challenge" end
    local bytes = new_tab(n, 0)

    for i = 1, n do

        local j = (i - 1) % l + 1
        bytes[i] = strchar(bxor(strbyte(password, i), strbyte(auth_data, j)))
    end

    local pub, err = rsa.new(public_key)

    if not pub then
        return nil, "new rsa err: " .. err
    end

    local enc, err = pub:encrypt(concat(bytes))
    if not enc then
        return nil, "encode password packet: " .. err
    end

    return enc
end

local function _write_encode_password(self, auth_data, public_key)
    local enc, err = _encrypt_password(self, auth_data, public_key)
    if not enc then
        return "failed to encrypt password: " .. err
    end

    local bytes, err = _send_packet(self, enc, #enc)
    if not bytes then
        return "failed to send encode password packet: " .. err
    end
end

local function _auth(self, auth_data, plugin)
    local password = self.password

    if plugin == "caching_sha2_password" then
        local auth_resp, err = _compute_sha256_token(password, auth_data)
        if err then
            return nil, "failed to compute sha256 token: " .. err
        end

        return auth_resp
    end

    if plugin == "mysql_clear_password" then
        if not self.is_unix and not self.use_ssl then
            return nil, "mysql_clear_password requires TLS or Unix socket"
        end
        return _to_cstring(password)
    end

    if plugin == "mysql_native_password" then
        return _compute_token(password, auth_data)
    end

    if plugin == "sha256_password" then
        if self.is_unix or self.use_ssl or #password == 0 then
            return _to_cstring(password)
        end

        local public_key = self.public_key
        if public_key then
            return _encrypt_password(self, auth_data, public_key)
        end

        return "\1"
    end

    if plugin == "client_ed25519" then

        auth_data = sub(auth_data, 1, 32)
        if #auth_data ~= 32 then return nil, "invalid client_ed25519 challenge" end
        local sig = ed25519.from_key_material(password):sign(auth_data)

        return sig
    end

    return nil, "unknown plugin: " .. plugin
end

local function _handle_auth_result(self, old_auth_data, plugin)
    local auth_data, new_plugin, err = _read_auth_result(self, old_auth_data,
                                                         plugin)

    if err ~= nil then
        local errno, sqlstate = auth_data, new_plugin
        return err, errno, sqlstate
    end

    if auth_data == RESP_OK then
        return
    end

    if new_plugin ~= "" then
        if not auth_data then
            auth_data = old_auth_data
        else
            old_auth_data = auth_data
        end

        plugin = new_plugin

        local auth_resp, err = _auth(self, auth_data, plugin)
        if not auth_resp then
            return err
        end

        local bytes, err = _send_packet(self, auth_resp, #auth_resp)
        if not bytes then
            return "failed to send client authentication packet: " .. err
        end

        auth_data, new_plugin, err = _read_auth_result(self, old_auth_data,
                                                       plugin)

        if err ~= nil then
            local errno, sqlstate = auth_data, new_plugin
            return err, errno, sqlstate
        end

        if auth_data == RESP_OK then
            return
        end

        if new_plugin ~= "" then
            return "malformed packet"
        end
    end

    if plugin == "caching_sha2_password" then
        local len = #auth_data
        if len == 0 then
            return
        end

        if len == 1 then
            local status = strbyte(auth_data)

            if status == 3 then
                return _read_ok_result(self)
            end

            if status == 4 then
                if self.is_unix or self.use_ssl then
                    local bytes, err = _send_packet(self,
                                                    _to_cstring(self.password),
                                                    #self.password + 1)

                    if not bytes then
                        return "failed to send cleartext auth packet: "
                            .. err
                    end

                else
                    local public_key = self.public_key
                    if not public_key then

                        local bytes, err = _send_packet(self, "\2", 1)
                        if not bytes then
                            return "failed to send password request packet: "
                                .. err
                        end

                        local packet, _, err = _recv_packet(self)
                        if not packet then
                            return "failed to receive the result packet: "
                                .. err
                        end

                        public_key = sub(packet, 2)
                    end

                    err = _write_encode_password(self, old_auth_data,
                                                 public_key)

                    if err then
                        return err
                    end

                    self.public_key = public_key
                end

                return _read_ok_result(self)
            end
        end

        return "malformed packet"
    end

    if plugin == "sha256_password" then
        if #auth_data ~= 0 then
            local err = _write_encode_password(self, old_auth_data, auth_data)

            if err then
                return err
            end

            return _read_ok_result(self)
        end
    end
    return "unexpected authentication response"
end

function _M.new()
    local sock = Socket.new()
    sock:settimeout(5000)
    return setmetatable({ sock = sock, _timeout = 5000, _reused = 0 }, mt)
end

function _M.set_timeout(self, timeout)
    local sock = self.sock
    if not sock then
        return nil, "not initialized"
    end

    if type(timeout) ~= "number" or timeout < 0 or timeout > 1e12
       or timeout ~= math.floor(timeout) then return nil, "invalid timeout" end
    sock:settimeout(timeout)
    self._timeout = timeout
    return true
end

local function options(value)
    if type(value) == "string" then return _M.parse_url(value) end
    if type(value) ~= "table" then return nil, "expected connection options or mysql:// URL" end
    local copy = {}
    for key, item in pairs(value) do copy[key] = item end
    return copy
end

function _M.connect(self, value)
    if self.state then return nil, "already connected" end
    local opts, err = options(value)
    if not opts then return nil, err end
    if opts.pool or opts.backlog then return nil, "use an explicit mysql.client for pooling; backlog is not supported" end
    local max_packet_size = opts.max_packet_size or 1024 * 1024
    if type(max_packet_size) ~= "number" or max_packet_size < 1024
       or max_packet_size > 1024 * 1024 * 1024 or max_packet_size ~= math.floor(max_packet_size) then
        return nil, "invalid max_packet_size"
    end
    self._max_packet_size = max_packet_size
    self.compact = opts.compact_arrays or false
    self.database = opts.database or ""
    self.user = opts.user or ""
    self.password = opts.password or ""
    for _, text in ipairs({ self.database, self.user, self.password }) do
        if type(text) ~= "string" or strfind(text, "\0", 1, true) then return nil, "invalid credentials or database" end
    end
    self.charset = CHARSET_MAP[opts.charset or "utf8mb4"]
    if not self.charset then return nil, "unsupported charset" end
    self.use_ssl = opts.ssl == true or opts.ssl_verify == true or opts.ctx ~= nil
    self.ssl_server_name = opts.ssl_server_name or opts.host or "localhost"
    self.ctx = opts.ctx
    if self.use_ssl and not self.ctx then
        self.ctx = TlsContext.new()
        if opts.ssl_verify == false then self.ctx:verify_none() end
    end
    self.public_key = opts.public_key
    self.is_unix = opts.path ~= nil
    self.packet_no = -1
    self.sock = self.sock or Socket.new()
    local ok
    ok, err = self:set_timeout(opts.timeout or self._timeout or 5000)
    if not ok then return nil, err end
    self.sock:setbufsize(max_packet_size)
    if self.is_unix then
        ok, err = self.sock:connect_unix(opts.path)
    else
        ok, err = self.sock:connect(opts.host or "127.0.0.1", opts.port or 3306)
    end
    if not ok then return nil, "failed to connect: " .. err end
    self.DEFAULT_CLIENT_FLAGS = bor(DEFAULT_CLIENT_FLAGS, CLIENT_PLUGIN_AUTH)
    local auth_data, plugin, errno, sqlstate
    auth_data, plugin, err, errno, sqlstate = _read_hand_shake_packet(self)
    if err then return nil, err, errno, sqlstate end
    self.DEFAULT_CLIENT_FLAGS = band(self.DEFAULT_CLIENT_FLAGS, self.capabilities)
    local auth_resp
    auth_resp, err = _auth(self, auth_data, plugin)
    if not auth_resp then return nil, err end
    err = _write_hand_shake_response(self, auth_resp, plugin)
    if err then return nil, err end
    err, errno, sqlstate = _handle_auth_result(self, auth_data, plugin)
    if err then return nil, err, errno, sqlstate end
    self.state = STATE_CONNECTED
    self.password = nil
    return 1
end

function _M.get_reused_times(self)
    if not self.sock then return nil, "not initialized" end
    return self._reused
end

function _M.close(self)
    local sock = self.sock
    if not sock then
        return nil, "not initialized"
    end

    self.state = nil
    self.packet_no = -1

    local bytes, err = _send_packet(self, strchar(COM_QUIT), 1)
    if not bytes then
        return nil, err
    end

    local ok, close_err = sock:close()
    self.sock = nil
    return ok, close_err
end

function _M.server_ver(self)
    return self._server_ver
end

local function send_query(self, query)
    self._binary=nil
    if self.state ~= STATE_CONNECTED then
        return nil, "cannot send query in the current context: "
                    .. (self.state or "nil")
    end

    if type(query) ~= "string" then return nil, "query must be a string" end
    self.packet_no = -1
    self.state = STATE_COMMAND_SENT
    local cmd_packet = strchar(COM_QUERY) .. query
    local packet_len = 1 + #query

    local bytes, err = _send_packet(self, cmd_packet, packet_len)
    if not bytes then
        return nil, err
    end

    return bytes
end
_M.send_query = send_query

local function read_result(self, est_nrows)
    if est_nrows~=nil and (type(est_nrows)~="number" or est_nrows<0 or est_nrows>100000 or est_nrows~=math.floor(est_nrows)) then return nil,"invalid estimated row count" end
    if self.state ~= STATE_COMMAND_SENT then
        return nil, "cannot read result in the current context: "
                    .. (self.state or "nil")
    end

    local sock = self.sock
    if not sock then
        return nil, "not initialized"
    end

    local packet, typ, err = _recv_packet(self)
    if not packet then
        return nil, err
    end

    if typ == RESP_ERR then
        self.state = STATE_CONNECTED

        local errno, msg, sqlstate = _parse_err_packet(packet)
        return nil, msg, errno, sqlstate
    end

    if typ == RESP_OK then
        local res = _parse_ok_packet(packet)
        self._server_status = res.server_status
        if res and band(res.server_status, SERVER_MORE_RESULTS_EXISTS) ~= 0 then
            return res, "again"
        end

        self.state = STATE_CONNECTED
        return res
    end

    if typ == RESP_LOCALINFILE then

        return nil, "packet type " .. typ .. " not supported"
    end

    local field_count, extra = _parse_result_set_header_packet(packet)

    if type(field_count)~="number" or field_count<1 or field_count>1024 or field_count~=math.floor(field_count) then return nil,"MySQL result exceeds 1024 columns" end
    local cols = new_tab(field_count, 0)
    for i = 1, field_count do
        local col, err, errno, sqlstate = _recv_field_packet(self)
        if not col then
            return nil, err, errno, sqlstate
        end

        cols[i] = col
    end

    local packet, typ, err = _recv_packet(self)
    if not packet then
        return nil, err
    end

    if typ ~= RESP_EOF then
        return nil, "unexpected packet type " .. typ .. " while eof packet is "
            .. "expected"
    end

    local compact = self.compact

    local rows = new_tab(est_nrows or 4, 0)
    local i,total = 0,0
    while true do

        packet, typ, err = _recv_packet(self)
        if not packet then
            return nil, err
        end

        if typ == RESP_EOF then
            local warning_count, status_flags = _parse_eof_packet(packet)
            self._server_status = status_flags

            if band(status_flags, SERVER_MORE_RESULTS_EXISTS) ~= 0 then
                return rows, "again"
            end

            break
        end

        if typ == RESP_ERR then
            self.state = STATE_CONNECTED
            local errno, msg, sqlstate = _parse_err_packet(packet)
            return nil, msg, errno, sqlstate
        end

        total=total+#packet
        if i>=100000 or total>64*1024*1024 then return nil,"MySQL result exceeds row or byte limit" end
        local row = (self._binary and _parse_binary_row or _parse_row_data_packet)(packet, cols, compact)
        i = i + 1
        rows[i] = row
    end

    self.state = STATE_CONNECTED

    return rows
end
_M.read_result = read_result

function _M.ping(self)
    if self.state ~= STATE_CONNECTED then return nil, "connection is not ready" end
    self.state = STATE_COMMAND_SENT
    self.packet_no = -1
    local sent, err = _send_packet(self, strchar(COM_PING), 1)
    if not sent then return nil, err end
    return read_result(self)
end

function _M.query(self, query, est_nrows)
    local bytes, err = send_query(self, query)
    if not bytes then
        return nil, "failed to send query: " .. err
    end

    return read_result(self, est_nrows)
end

function _M.set_compact_arrays(self, value)
    self.compact = value
end

local function discard(self)
    if self.sock then
        local ok = pcall(self.sock.abort, self.sock)
        if not ok then return nil, "connection is busy" end
    end
    Pool.finish(self._lease);self._lease=nil
    self._prepared_ids=nil
    self.sock, self.state, self._busy, self._owner = nil, nil, false, nil
    return 1
end

local Statement = {};Statement.__index=Statement
local function close_statement(self,stmt)
    if self.state~=STATE_CONNECTED then return nil,'connection is not ready' end
    if stmt._closed or not self._prepared_ids or not self._prepared_ids[stmt._id] then return nil,'statement is closed' end
    self.state=STATE_COMMAND_SENT;self.packet_no=-1
    local payload=strchar(0x19).._set_byte4(stmt._id)
    local sent,err=_send_packet(self,payload,#payload);if not sent then return nil,err end
    self.state=STATE_CONNECTED;self._prepared_ids[stmt._id]=nil;stmt._closed=true;return 1
end
function _M.prepare(self,sql)
    if self.state~=STATE_CONNECTED then return nil,'connection is not ready' end
    if type(sql)~='string' then return nil,'expected SQL string' end
    if #sql+1>self._max_packet_size then return nil,'prepared SQL exceeds packet limit' end
    local count=0;for _ in pairs(self._prepared_ids or {}) do count=count+1 end
    if count>=128 then return nil,'connection exceeds 128 open prepared statements' end
    self.state=STATE_COMMAND_SENT;self.packet_no=-1
    local sent,err=_send_packet(self,strchar(0x16)..sql,#sql+1);if not sent then return nil,err end
    local packet,typ,read_error=_recv_packet(self);if not packet then return nil,read_error end
    if typ==RESP_ERR then self.state=STATE_CONNECTED;local errno,msg,sqlstate=_parse_err_packet(packet);return nil,msg,errno,sqlstate end
    if typ~=RESP_OK or (#packet~=10 and #packet~=12) or strbyte(packet,10)~=0 then return nil,'invalid MySQL prepare response' end
    local id=_get_byte4(packet,2);local columns=_get_byte2(packet,6);local params=_get_byte2(packet,8)
    if columns>1024 or params>1024 then return nil,'prepared statement exceeds 1024 columns or parameters' end
    for _,number in ipairs({params,columns}) do
        for _=1,number do local col,error_message=_recv_field_packet(self);if not col then return nil,error_message end end
        if number>0 then local eof,eof_type,eof_error=_recv_packet(self);if not eof then return nil,eof_error end;if eof_type~=RESP_EOF then return nil,'missing prepared metadata EOF' end end
    end
    self._prepared_ids=self._prepared_ids or {};self._prepared_ids[id]=true;self.state=STATE_CONNECTED
    return setmetatable({_connection=self,_id=id,_parameters=params},Statement)
end
function _M._execute_stmt(self,stmt,params,est_nrows)
    if self.state~=STATE_CONNECTED then return nil,'connection is not ready' end
    if stmt._closed or not self._prepared_ids or not self._prepared_ids[stmt._id] then return nil,'statement is closed' end
    params=params or {}
    if type(params)~='table' then return nil,'expected parameter array' end
    local count=0;for key in pairs(params) do if type(key)~='number' or key<1 or key>stmt._parameters or key~=math.floor(key) then return nil,'invalid parameter array index' end;count=count+1 end
    if count~=stmt._parameters then return nil,'parameter count mismatch; use cosmos.null instead of nil' end
    local bitmap={};for i=1,math.floor((stmt._parameters+7)/8) do bitmap[i]=0 end
    local types,values={},{}
    local total=11+#bitmap+2*stmt._parameters
    for i=1,stmt._parameters do
        local value=rawget(params,i);local kind=type(value)
        local n=kind=="string" and #value or 0
        total=total+(kind=="number" and 8 or kind=="boolean" and 1 or kind=="string" and n+(n<=250 and 1 or n<=65535 and 3 or n<=16777215 and 4 or 9) or 0)
        if total>self._max_packet_size then return nil,"prepared parameter payload exceeds packet limit" end
        if value==null then local index=math.floor((i-1)/8)+1;bitmap[index]=bor(bitmap[index],lshift(1,(i-1)%8));types[i]=strchar(6,0)
        elseif kind=='boolean' then types[i]=strchar(1,0);values[#values+1]=value and '\1' or '\0'
        elseif kind=='number' then local numeric_type,encoded=_M._encode_number(value);if not numeric_type then return nil,encoded end;types[i]=strchar(numeric_type,0);values[#values+1]=encoded
        elseif kind=='string' then types[i]=strchar(253,0);values[#values+1]=_lenenc_string(value)
        else return nil,'unsupported MySQL parameter type' end
    end
    local payload=strchar(0x17).._set_byte4(stmt._id)..'\0'.._set_byte4(1)
    if stmt._parameters>0 then
        for i=1,#bitmap do bitmap[i]=strchar(bitmap[i]) end
        payload=payload..concat(bitmap)..'\1'..concat(types)..concat(values)
    end
    self.state=STATE_COMMAND_SENT;self.packet_no=-1;self._binary=true
    local sent,err=_send_packet(self,payload,#payload);if not sent then return nil,err end
    return read_result(self,est_nrows)
end
_M._close_stmt=close_statement
function _M._close_statements(self)
    if self.state~=STATE_CONNECTED then return nil,'connection is not ready' end
    for id in pairs(self._prepared_ids or {}) do local ok,err=close_statement(self,{_id=id});if not ok then return nil,err end end
    return 1
end
function Statement:param_count() return self._parameters end
function Statement:execute(params,est_nrows) return self._connection:_execute_stmt(self,params,est_nrows) end
function Statement:close()
    if self._closed then return 1 end
    return self._connection:_close_stmt(self)
end

local function pack(...) return { n = select("#", ...), ... } end

local function invoke(self, operation, ...)
    if self._busy then return nil, "connection is busy" end
    self._busy = true
    local result = pack(pcall(operation, self, ...))
    self._busy = false
    if not result[1] then
        discard(self)
        return nil, "MySQL protocol error: " .. tostring(result[2])
    end
    if not result[2] and self.state ~= STATE_CONNECTED then discard(self) end
    return unpack(result, 2, result.n)
end

for _, name in ipairs({"connect", "send_query", "read_result", "query", "ping", "prepare", "_execute_stmt", "_close_stmt", "_close_statements"}) do
    local operation = _M[name]
    _M[name] = function(self, ...) return invoke(self, operation, ...) end
end
local close = _M.close
function _M.close(self)
    if self._busy then return discard(self) end
    local result, err = invoke(self, close)
    discard(self)
    return result, err
end

-- 十六进制 SQL 字符串字面量不依赖反斜线转义模式，也支持 NUL 和任意二进制。
function _M.quote_sql_str(value)
    if type(value) ~= "string" then return nil, "expected a string" end
    return "X'" .. hex.encode(value) .. "'"
end

local Client = {}
Client.__index = Client

function _M.client(value)
    local opts, err = options(value)
    if not opts then return nil, err end
    if opts.pool or opts.backlog then return nil, "named pools and backlog are not supported" end
    local size = opts.pool_size or 10
    local idle_timeout = opts.idle_timeout or 60000
    if type(size) ~= "number" or size < 1 or size > 65536 or size ~= math.floor(size) then
        return nil, "invalid pool_size"
    end
    if type(idle_timeout) ~= "number" or idle_timeout < 0 or idle_timeout > 1e12
       or idle_timeout ~= idle_timeout then
        return nil, "invalid idle_timeout"
    end
    return Pool.attach(setmetatable({ _options = opts, _size = size, _idle_timeout = idle_timeout,
                          _idle = {}, _closed = false }, Client),opts)
end

function Client:connect()
    local lease,acquire_error=Pool.acquire(self)
    if not lease then return nil,acquire_error end
    while #self._idle > 0 do
        local idle = table.remove(self._idle)
        local conn = idle.connection
        if (self._idle_timeout == 0 or idle.since:elapsed_ms() < self._idle_timeout)
           and conn.sock:state() == 0 then
            conn:set_timeout(self._options.timeout or 5000)
            conn.compact = self._options.compact_arrays or false
            local ok = conn:ping()
            if ok then
                if self._closed then discard(conn);Pool.finish(lease); return nil, "client is closed" end
                conn._owner = self
                conn._lease = lease
                conn._reused = conn._reused + 1
                return conn
            end
        end
        discard(conn)
    end
    if self._closed then Pool.finish(lease);return nil,"client is closed" end
    local conn = _M.new()
    conn._lease=lease
    local ok, err, errno, sqlstate = conn:connect(self._options)
    if not ok then discard(conn);Pool.finish(lease);return nil, err, errno, sqlstate end
    if self._closed then discard(conn);Pool.finish(lease); return nil, "client is closed" end
    conn._owner = self
    return conn
end

function Client:release(conn)
    if type(conn)=="table" and conn._scope then return nil,"connection belongs to an active scope" end
    if getmetatable(conn) ~= mt or conn._owner ~= self then return nil, "connection belongs to another client" end
    if conn._busy or conn.state ~= STATE_CONNECTED then return nil, "connection has unread results or is busy" end
    if not conn.sock or conn.sock:state() ~= 0 then discard(conn); return nil, "closed" end
    if band(conn._server_status or 0, 1) ~= 0 or band(conn._server_status or 0, 2) == 0 then
        return nil, "transaction must be completed before pooling"
    end
    if self._closed then discard(conn); return nil, "client is closed" end
    local closed,close_error=conn:_close_statements();if not closed then discard(conn);return nil,close_error end
    if self._closed then discard(conn);return nil,"client is closed" end
    if #self._idle >= self._size then discard(conn); return 1 end
    -- 转移到一张从未交给调用方的表，旧句柄立即失效，避免归还后的别名操作新租借。
    local parked = {}
    for key, item in pairs(conn) do parked[key] = item end
    parked._owner,parked._lease = nil,nil
    Pool.finish(conn._lease);conn._lease=nil
    conn.sock, conn.state, conn._owner = nil, nil, nil
    self._idle[#self._idle + 1] = {connection = setmetatable(parked, mt), since = cosmos.time.instant.now()}
    return 1
end

function _M.set_keepalive(self)
    if not self._owner then return nil, "connection does not belong to a client" end
    return self._owner:release(self)
end

function Client:close()
    if self._closed then return end
    self._closed = true
    Pool.close(self)
    for _, idle in ipairs(self._idle) do discard(idle.connection) end
    self._idle = {}
end

function Client:stats() return Pool.stats(self) end
function Client:with_connection(callback,...) return Pool.scope(self,callback,nil,...) end
function Client:transaction(callback,...)
    return Pool.scope(self,callback,{
        begin=function(c)return c:query("START TRANSACTION") end,
        commit=function(c)return c:query("COMMIT") end,
        rollback=function(c)return c:query("ROLLBACK") end,
    },...)
end
return _M
