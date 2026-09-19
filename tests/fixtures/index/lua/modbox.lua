#!/usr/bin/env lua
-- Representative Lua fixture: function forms, require, calls.

local protocol = require("protocol")
local util = require 'util'
local dyn = require(prefix .. ".mod")

function foo(a, b)
    return a + b
end

local function helper(x)
    return x * 2
end

function module.fn()
end

function object:method()
end

M.send(packet)
obj:call()
plain()
