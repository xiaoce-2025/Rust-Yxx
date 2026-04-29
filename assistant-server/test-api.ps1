#!/usr/bin/env powershell
# Simple API Test Script for Rust Assistant Server
# Usage: .\test-api.ps1 -Test all

param(
    [string]$Test = "all",
    [string]$BaseUrl = "http://127.0.0.1:8080"
)

$ErrorActionPreference = "Stop"
$ColorSuccess = "Green"
$ColorError = "Red"  
$ColorInfo = "Cyan"
$ColorWarn = "Yellow"

$Global:Token = $null

function Test-Health {
    Write-Host "`n[TEST] Health Check" -ForegroundColor $ColorWarn
    try {
        $resp = Invoke-WebRequest -Uri "$BaseUrl/health" -UseBasicParsing
        Write-Host "PASS: Health check OK" -ForegroundColor $ColorSuccess
        return $true
    } catch {
        Write-Host "FAIL: $_" -ForegroundColor $ColorError
        return $false
    }
}

function Test-Register {
    Write-Host "`n[TEST] User Registration" -ForegroundColor $ColorWarn
    try {
        $email = "test-$(Get-Random)@example.com"
        $body = @{
            username = "user$(Get-Random)"
            email = $email
            password = "Pass123!"
        } | ConvertTo-Json
        
        $resp = Invoke-WebRequest -Uri "$BaseUrl/api/auth/register" `
            -Method Post -Body $body -ContentType "application/json" -UseBasicParsing
        
        $data = $resp.Content | ConvertFrom-Json
        $Global:Token = $data.token
        
        Write-Host "PASS: User created - Email=$email" -ForegroundColor $ColorSuccess
        return $true
    } catch {
        Write-Host "FAIL: $_" -ForegroundColor $ColorError
        return $false
    }
}

function Test-Login {
    Write-Host "`n[TEST] User Login" -ForegroundColor $ColorWarn
    try {
        $email = "login$(Get-Random)@example.com"
        $password = "Pass123!"
        
        $regBody = @{
            username = "user$(Get-Random)"
            email = $email
            password = $password
        } | ConvertTo-Json
        
        Invoke-WebRequest -Uri "$BaseUrl/api/auth/register" `
            -Method Post -Body $regBody -ContentType "application/json" -UseBasicParsing | Out-Null
        
        $loginBody = @{ email = $email; password = $password } | ConvertTo-Json
        
        $resp = Invoke-WebRequest -Uri "$BaseUrl/api/auth/login" `
            -Method Post -Body $loginBody -ContentType "application/json" -UseBasicParsing
        
        $data = $resp.Content | ConvertFrom-Json
        $Global:Token = $data.token
        
        Write-Host "PASS: Login successful - Token=$($data.token.Substring(0,8))..." -ForegroundColor $ColorSuccess
        return $true
    } catch {
        Write-Host "FAIL: $_" -ForegroundColor $ColorError
        return $false
    }
}

function Test-GetMe {
    Write-Host "`n[TEST] Get Current User" -ForegroundColor $ColorWarn
    if (-not $Global:Token) { 
        Write-Host "SKIP: No token, run register/login first" -ForegroundColor $ColorWarn
        return $false 
    }
    
    try {
        $headers = @{ "Authorization" = "Bearer $($Global:Token)" }
        $resp = Invoke-WebRequest -Uri "$BaseUrl/api/auth/me" -Headers $headers -UseBasicParsing
        $data = $resp.Content | ConvertFrom-Json
        
        Write-Host "PASS: User=$($data.username), Email=$($data.email)" -ForegroundColor $ColorSuccess
        return $true
    } catch {
        Write-Host "FAIL: $_" -ForegroundColor $ColorError
        return $false
    }
}

function Test-SendMessage {
    Write-Host "`n[TEST] Send Message" -ForegroundColor $ColorWarn
    if (-not $Global:Token) { 
        Write-Host "SKIP: No token, run register/login first" -ForegroundColor $ColorWarn
        return $false 
    }
    
    try {
        $headers = @{ "Authorization" = "Bearer $($Global:Token)" }
        $body = @{ text = "Hello from test!"; conversation_id = $null } | ConvertTo-Json
        
        $resp = Invoke-WebRequest -Uri "$BaseUrl/api/assistant/send" `
            -Method Post -Headers $headers -Body $body -ContentType "application/json" -UseBasicParsing
        
        $data = $resp.Content | ConvertFrom-Json
        Write-Host "PASS: Message sent - ConvID=$($data.conversation_id)" -ForegroundColor $ColorSuccess
        return $true
    } catch {
        Write-Host "FAIL: $_" -ForegroundColor $ColorError
        return $false
    }
}

function Test-GetConversations {
    Write-Host "`n[TEST] Get Conversations" -ForegroundColor $ColorWarn
    if (-not $Global:Token) { 
        Write-Host "SKIP: No token" -ForegroundColor $ColorWarn
        return $false 
    }
    
    try {
        $headers = @{ "Authorization" = "Bearer $($Global:Token)" }
        $resp = Invoke-WebRequest -Uri "$BaseUrl/api/conversations" -Headers $headers -UseBasicParsing
        $data = $resp.Content | ConvertFrom-Json
        
        Write-Host "PASS: Found $($data.Count) conversations" -ForegroundColor $ColorSuccess
        return $true
    } catch {
        Write-Host "FAIL: $_" -ForegroundColor $ColorError
        return $false
    }
}

function Test-GetSettings {
    Write-Host "`n[TEST] Get Settings" -ForegroundColor $ColorWarn
    if (-not $Global:Token) { 
        Write-Host "SKIP: No token" -ForegroundColor $ColorWarn
        return $false 
    }
    
    try {
        $headers = @{ "Authorization" = "Bearer $($Global:Token)" }
        $resp = Invoke-WebRequest -Uri "$BaseUrl/api/settings/profile" -Headers $headers -UseBasicParsing
        $data = $resp.Content | ConvertFrom-Json
        
        Write-Host "PASS: Model=$($data.default_model), Theme=$($data.theme)" -ForegroundColor $ColorSuccess
        return $true
    } catch {
        Write-Host "FAIL: $_" -ForegroundColor $ColorError
        return $false
    }
}

function Test-UpdateSettings {
    Write-Host "`n[TEST] Update Settings" -ForegroundColor $ColorWarn
    if (-not $Global:Token) { 
        Write-Host "SKIP: No token" -ForegroundColor $ColorWarn
        return $false 
    }
    
    try {
        $headers = @{ "Authorization" = "Bearer $($Global:Token)" }
        $body = @{ default_model = "gpt-4"; theme = "dark" } | ConvertTo-Json
        
        $resp = Invoke-WebRequest -Uri "$BaseUrl/api/settings/profile" `
            -Method Put -Headers $headers -Body $body -ContentType "application/json" -UseBasicParsing
        
        $data = $resp.Content | ConvertFrom-Json
        Write-Host "PASS: Updated - Model=$($data.default_model), Theme=$($data.theme)" -ForegroundColor $ColorSuccess
        return $true
    } catch {
        Write-Host "FAIL: $_" -ForegroundColor $ColorError
        return $false
    }
}

function Test-WeChatStatus {
    Write-Host "`n[TEST] WeChat Binding Status" -ForegroundColor $ColorWarn
    if (-not $Global:Token) { 
        Write-Host "SKIP: No token" -ForegroundColor $ColorWarn
        return $false 
    }
    
    try {
        $headers = @{ "Authorization" = "Bearer $($Global:Token)" }
        $resp = Invoke-WebRequest -Uri "$BaseUrl/api/wechat/binding-status" -Headers $headers -UseBasicParsing
        $data = $resp.Content | ConvertFrom-Json
        
        $status = if ($data.bound) { "Bound" } else { "Not Bound" }
        Write-Host "PASS: WeChat status=$status" -ForegroundColor $ColorSuccess
        return $true
    } catch {
        Write-Host "FAIL: $_" -ForegroundColor $ColorError
        return $false
    }
}

function Run-AllTests {
    Write-Host "`n======================================" -ForegroundColor $ColorWarn
    Write-Host "Rust Assistant Server - API Tests" -ForegroundColor $ColorWarn
    Write-Host "Base URL: $BaseUrl" -ForegroundColor $ColorInfo
    Write-Host "======================================" -ForegroundColor $ColorWarn
    
    $results = @()
    $results += Test-Health
    $results += Test-Register
    $results += Test-GetMe
    $results += Test-SendMessage
    $results += Test-GetConversations
    $results += Test-GetSettings
    $results += Test-UpdateSettings
    $results += Test-WeChatStatus
    
    $passed = ($results | Where-Object { $_ }).Count
    $total = $results.Count
    
    Write-Host "`n======================================" -ForegroundColor $ColorWarn
    Write-Host "Results: $passed/$total tests passed" -ForegroundColor $ColorSuccess
    Write-Host "======================================" -ForegroundColor $ColorWarn
}

switch ($Test.ToLower()) {
    "all" { Run-AllTests }
    "health" { Test-Health }
    "register" { Test-Register }
    "login" { Test-Login }
    "me" { Test-Register; Test-GetMe }
    "message" { Test-Register; Test-SendMessage }
    "conversations" { Test-Register; Test-GetConversations }
    "settings" { Test-Register; Test-GetSettings; Test-UpdateSettings }
    "wechat" { Test-Register; Test-WeChatStatus }
    default { 
        Write-Host "Tests available: all, health, register, login, me, message, conversations, settings, wechat" -ForegroundColor $ColorInfo
        Run-AllTests 
    }
}
