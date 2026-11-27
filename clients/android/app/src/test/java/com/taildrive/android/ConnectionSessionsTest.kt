package com.taildrive.android

import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class ConnectionSessionsTest {
    @Test fun oneSessionIsReusedUntilTheConnectionChanges() {
        var opened = 0
        val sessions = ConnectionSessions<FakeSession> { FakeSession(opened++) }
        val account = connection("100.64.0.1", "alpha")
        val first = sessions.withSession(account) { it }
        val again = sessions.withSession(Connection(account.url, account.token)) { it }
        assertSame(first, again)
        assertFalse(first.closed)

        val other = sessions.withSession(connection("100.97.87.111", "beta")) { it }
        assertTrue(first.closed)
        assertFalse(other.closed)
        val reopened = sessions.withSession(account) { it }
        assertTrue(other.closed)
        assertFalse(reopened.closed)
        assertNotEquals(first.id, reopened.id)
    }

    @Test fun aFailedOpenLeavesTheCurrentSessionCached() {
        var fail = false
        var opened = 0
        val sessions = ConnectionSessions<FakeSession> {
            if (fail) throw IllegalStateException("runtime unavailable")
            FakeSession(opened++)
        }
        val account = connection("100.64.0.1", "alpha")
        val current = sessions.withSession(account) { it }
        fail = true
        val failure = assertThrows(IllegalStateException::class.java) {
            sessions.withSession(connection("100.97.87.111", "beta")) { error("opened") }
        }
        assertEquals("runtime unavailable", failure.message)
        assertFalse(current.closed)
        assertSame(current, sessions.withSession(account) { it })
        fail = false
        val created = sessions.withSession(connection("100.97.87.111", "beta")) { it }
        assertTrue(current.closed)
        assertFalse(created.closed)
    }

    @Test fun overlappingCallsShareTheSessionAndAnIdleAccountIsClosedAfterward() {
        val opened = AtomicInteger()
        val sessions = ConnectionSessions<FakeSession> { FakeSession(opened.incrementAndGet()) }
        val first = connection("100.64.0.1", "alpha")
        val second = connection("100.97.87.111", "beta")
        val entered = CountDownLatch(2)
        val leave = CountDownLatch(1)
        val seen = mutableListOf<FakeSession>()
        val workers = List(2) {
            Thread {
                sessions.withSession(first) { session ->
                    synchronized(seen) { seen.add(session) }
                    entered.countDown()
                    check(leave.await(5, TimeUnit.SECONDS))
                }
            }
        }
        workers.forEach { it.start() }
        assertTrue(entered.await(5, TimeUnit.SECONDS))
        val active = synchronized(seen) { seen.toList() }
        assertSame(active[0], active[1])
        assertFalse(active[0].closed)

        val switched = sessions.withSession(second) { it }
        assertFalse(active[0].closed)
        assertFalse(switched.closed)
        leave.countDown()
        workers.forEach { thread ->
            thread.join(5_000)
            assertFalse(thread.isAlive)
        }
        assertTrue(active[0].closed)
        assertFalse(switched.closed)
        assertSame(switched, sessions.withSession(second) { it })
    }

    private fun connection(host: String, token: String) = Connection("http://$host:8787/", token)

    private class FakeSession(val id: Int) : AutoCloseable {
        var closed = false
            private set

        override fun close() {
            check(!closed) { "session $id closed twice" }
            closed = true
        }
    }
}
