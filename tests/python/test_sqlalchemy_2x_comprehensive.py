#!/usr/bin/env python3
"""
Comprehensive SQLAlchemy 2.x multi-driver compatibility suite for pgsqlite.

Covers psycopg2, psycopg (v3 text & binary), and asyncpg across:
- DDL (ENUM, UUID, ARRAY, JSONB, Numeric(p,s), Boolean, DateTime(tz=True), constraints/indexes)
- Core UPSERT (`ON CONFLICT ... DO UPDATE ... RETURNING`)
- ORM CRUD, relationships, and exact Python round-trip types
- Explicit nested savepoints (`begin_nested()` + rollback & error recovery via SQLSTATE 23xxx)
- PostgreSQL `= ANY(...)` array predicates and aggregate queries
- Schema reflection via `sqlalchemy.inspect(...)` across tables, columns, PKs, FKs, indexes,
  unique/check constraints, and enums
"""

import argparse
import asyncio
import datetime
import decimal
import enum
import os
import socket
import subprocess
import sys
import tempfile
import time
import traceback
import uuid

from sqlalchemy import (
    Boolean,
    CheckConstraint,
    DateTime,
    Enum as SAEnum,
    ForeignKey,
    Index,
    Integer,
    Numeric,
    String,
    UniqueConstraint,
    any_,
    create_engine,
    func,
    inspect,
    select,
)
from sqlalchemy.dialects.postgresql import ARRAY, JSONB, UUID as PGUUID, insert as pg_insert
from sqlalchemy.exc import IntegrityError
from sqlalchemy.ext.asyncio import AsyncSession, create_async_engine
from sqlalchemy.orm import DeclarativeBase, Mapped, Session, mapped_column, relationship


REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


class StatusEnum(enum.Enum):
    PENDING = "PENDING"
    ACTIVE = "ACTIVE"


class Base(DeclarativeBase):
    pass


class Item(Base):
    __tablename__ = "items"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ext_id: Mapped[uuid.UUID] = mapped_column(PGUUID(as_uuid=True), nullable=False)
    name: Mapped[str] = mapped_column(String(100), nullable=False)
    tags: Mapped[list[str]] = mapped_column(ARRAY(String), nullable=False)
    meta: Mapped[dict] = mapped_column(JSONB, nullable=False)
    status: Mapped[StatusEnum] = mapped_column(SAEnum(StatusEnum, name="status_enum"), nullable=False)
    balance: Mapped[decimal.Decimal] = mapped_column(Numeric(10, 2), nullable=False)
    active: Mapped[bool] = mapped_column(Boolean, nullable=False, default=True)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime(timezone=True), nullable=False)
    children = relationship("Child", back_populates="parent", cascade="all, delete-orphan")

    __table_args__ = (
        CheckConstraint("balance >= 0", name="ck_balance_nonneg"),
        UniqueConstraint("name", name="uq_items_name"),
        Index("ix_items_created", "created_at"),
    )


class Child(Base):
    __tablename__ = "children"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    parent_id: Mapped[int] = mapped_column(ForeignKey("items.id", ondelete="CASCADE"), nullable=False)
    label: Mapped[str] = mapped_column(String(50), nullable=False)
    parent = relationship("Item", back_populates="children")


def run_reflection_checks(sync_conn) -> None:
    insp = inspect(sync_conn)
    schemas = insp.get_schema_names()
    assert "public" in schemas, f"schemas={schemas}"

    tables = insp.get_table_names()
    assert "items" in tables and "children" in tables, f"tables={tables}"

    views = insp.get_view_names()
    assert isinstance(views, list), f"views={views}"
    assert insp.has_table("items"), "has_table('items') false"

    cols = insp.get_columns("items")
    col_names = [c["name"] for c in cols]
    for expected in ["id", "ext_id", "name", "tags", "meta", "status", "balance", "active", "created_at"]:
        assert expected in col_names, f"missing col {expected} in {col_names}"

    pk = insp.get_pk_constraint("items")
    assert pk.get("constrained_columns") == ["id"], f"pk={pk}"

    fks = insp.get_foreign_keys("children")
    assert len(fks) == 1 and fks[0]["referred_table"] == "items" and fks[0]["referred_columns"] == ["id"], f"fks={fks}"

    idxs = insp.get_indexes("items")
    assert any(ix["name"] == "ix_items_created" for ix in idxs), f"idxs={idxs}"

    uqs = insp.get_unique_constraints("items")
    assert any("name" in u["column_names"] for u in uqs), f"uqs={uqs}"

    cks = insp.get_check_constraints("items")
    assert any(c["name"] == "ck_balance_nonneg" for c in cks), f"cks={cks}"

    enums = insp.get_enums()
    assert any(e["name"] == "status_enum" and set(e["labels"]) == {"PENDING", "ACTIVE"} for e in enums), f"enums={enums}"


def check_sync_driver(name: str, url: str, connect_args=None) -> dict[str, str]:
    print(f"\n=== Testing {name} ===")
    kwargs = {}
    if connect_args:
        kwargs["connect_args"] = connect_args
    engine = create_engine(url, **kwargs)
    passed: dict[str, str] = {}

    def step(label, fn):
        try:
            fn()
            passed[label] = "PASS"
            print(f"  [PASS] {label}")
        except Exception as exc:
            passed[label] = f"FAIL: {exc}"
            print(f"  [FAIL] {label}: {exc}")
            traceback.print_exc()

    def s_ddl():
        Base.metadata.drop_all(engine)
        Base.metadata.create_all(engine)

    uid1 = uuid.uuid4()
    ts1 = datetime.datetime(2025, 3, 15, 12, 30, 0, tzinfo=datetime.timezone.utc)

    def s_core_upsert():
        with engine.begin() as conn:
            stmt = pg_insert(Item.__table__).values(
                ext_id=uid1,
                name="alpha",
                tags=["a", "b"],
                meta={"k": 1, "nested": [True, False]},
                status=StatusEnum.PENDING,
                balance=decimal.Decimal("12.34"),
                active=True,
                created_at=ts1,
            ).on_conflict_do_update(
                index_elements=["name"],
                set_={"balance": decimal.Decimal("99.95")},
            ).returning(Item.id, Item.balance)
            r1 = conn.execute(stmt).one()
            assert r1.balance == decimal.Decimal("12.34"), f"r1={r1}"

            stmt2 = pg_insert(Item.__table__).values(
                ext_id=uid1,
                name="alpha",
                tags=["a", "b"],
                meta={"k": 1},
                status=StatusEnum.PENDING,
                balance=decimal.Decimal("50.00"),
                active=True,
                created_at=ts1,
            ).on_conflict_do_update(
                index_elements=["name"],
                set_={"balance": decimal.Decimal("99.95")},
            ).returning(Item.id, Item.balance)
            r2 = conn.execute(stmt2).one()
            assert r2.balance == decimal.Decimal("99.95"), f"r2={r2}"

    def s_orm_crud():
        uid2 = uuid.uuid4()
        with Session(engine) as s:
            it = Item(
                ext_id=uid2,
                name="beta",
                tags=["x", "y"],
                meta={"role": "admin"},
                status=StatusEnum.ACTIVE,
                balance=decimal.Decimal("200.50"),
                active=False,
                created_at=ts1,
                children=[Child(label="c1"), Child(label="c2")],
            )
            s.add(it)
            s.commit()
            loaded = s.execute(select(Item).where(Item.name == "beta")).scalar_one()
            assert loaded.ext_id == uid2, f"ext_id={loaded.ext_id}"
            assert loaded.tags == ["x", "y"], f"tags={loaded.tags}"
            assert loaded.meta == {"role": "admin"}, f"meta={loaded.meta}"
            assert loaded.status == StatusEnum.ACTIVE, f"status={loaded.status}"
            assert loaded.balance == decimal.Decimal("200.50"), f"balance={loaded.balance}"
            assert loaded.active is False, f"active={loaded.active}"
            assert len(loaded.children) == 2

    def s_savepoint_ok():
        with Session(engine) as s:
            with s.begin():
                with s.begin_nested() as sp:
                    s.add(Item(
                        ext_id=uuid.uuid4(),
                        name="temp_sp",
                        tags=["t"],
                        meta={},
                        status=StatusEnum.PENDING,
                        balance=decimal.Decimal("1.00"),
                        active=True,
                        created_at=ts1,
                    ))
                    s.flush()
                    sp.rollback()
                assert s.execute(select(Item).where(Item.name == "temp_sp")).scalar_one_or_none() is None

    def s_savepoint_err():
        with Session(engine) as s:
            with s.begin():
                try:
                    with s.begin_nested():
                        s.add(Item(
                            ext_id=uuid.uuid4(),
                            name="alpha",
                            tags=["dup"],
                            meta={},
                            status=StatusEnum.PENDING,
                            balance=decimal.Decimal("1.00"),
                            active=True,
                            created_at=ts1,
                        ))
                        s.flush()
                except IntegrityError:
                    pass
                cnt = s.execute(select(func.count(Item.id))).scalar_one()
                assert cnt >= 2, f"cnt={cnt}"

    def s_array_and_agg():
        with Session(engine) as s:
            hits = s.execute(select(Item).where(any_(Item.tags) == "a")).scalars().all()
            assert len(hits) == 1 and hits[0].name == "alpha", f"hits={hits}"
            cnt = s.execute(select(func.count(Item.id))).scalar_one()
            assert cnt == 2, f"cnt={cnt}"

    def s_reflect():
        with engine.connect() as conn:
            run_reflection_checks(conn)

    step("ddl", s_ddl)
    step("core_upsert_returning", s_core_upsert)
    step("orm_crud_types", s_orm_crud)
    step("savepoint_rollback", s_savepoint_ok)
    step("savepoint_after_error", s_savepoint_err)
    step("array_any_and_agg", s_array_and_agg)
    step("inspect_reflection", s_reflect)
    engine.dispose()
    return passed


async def check_asyncpg(url: str) -> dict[str, str]:
    print("\n=== Testing asyncpg ===")
    engine = create_async_engine(url)
    passed: dict[str, str] = {}

    async def step(label, coro):
        try:
            await coro()
            passed[label] = "PASS"
            print(f"  [PASS] {label}")
        except Exception as exc:
            passed[label] = f"FAIL: {exc}"
            print(f"  [FAIL] {label}: {exc}")
            traceback.print_exc()

    async def a_ddl():
        async with engine.begin() as conn:
            await conn.run_sync(Base.metadata.drop_all)
            await conn.run_sync(Base.metadata.create_all)

    uid1 = uuid.uuid4()
    ts1 = datetime.datetime(2025, 3, 15, 12, 30, 0, tzinfo=datetime.timezone.utc)

    async def a_core_upsert():
        async with engine.begin() as conn:
            stmt = pg_insert(Item.__table__).values(
                ext_id=uid1,
                name="alpha",
                tags=["a", "b"],
                meta={"k": 1, "nested": [True, False]},
                status=StatusEnum.PENDING,
                balance=decimal.Decimal("12.34"),
                active=True,
                created_at=ts1,
            ).on_conflict_do_update(
                index_elements=["name"],
                set_={"balance": decimal.Decimal("99.95")},
            ).returning(Item.id, Item.balance)
            r1 = (await conn.execute(stmt)).one()
            assert r1.balance == decimal.Decimal("12.34")

            stmt2 = pg_insert(Item.__table__).values(
                ext_id=uid1,
                name="alpha",
                tags=["a", "b"],
                meta={"k": 1},
                status=StatusEnum.PENDING,
                balance=decimal.Decimal("50.00"),
                active=True,
                created_at=ts1,
            ).on_conflict_do_update(
                index_elements=["name"],
                set_={"balance": decimal.Decimal("99.95")},
            ).returning(Item.id, Item.balance)
            r2 = (await conn.execute(stmt2)).one()
            assert r2.balance == decimal.Decimal("99.95")

    async def a_orm_crud():
        uid2 = uuid.uuid4()
        async with AsyncSession(engine) as s:
            async with s.begin():
                s.add(Item(
                    ext_id=uid2,
                    name="beta",
                    tags=["x", "y"],
                    meta={"role": "admin"},
                    status=StatusEnum.ACTIVE,
                    balance=decimal.Decimal("200.50"),
                    active=False,
                    created_at=ts1,
                    children=[Child(label="c1"), Child(label="c2")],
                ))
        async with AsyncSession(engine) as s:
            loaded = (await s.execute(select(Item).where(Item.name == "beta"))).scalar_one()
            assert loaded.ext_id == uid2
            assert loaded.tags == ["x", "y"]
            assert loaded.meta == {"role": "admin"}
            assert loaded.status == StatusEnum.ACTIVE
            assert loaded.balance == decimal.Decimal("200.50")
            assert loaded.active is False

    async def a_savepoint_ok():
        async with AsyncSession(engine) as s:
            async with s.begin():
                async with s.begin_nested() as sp:
                    s.add(Item(
                        ext_id=uuid.uuid4(),
                        name="temp_sp",
                        tags=["t"],
                        meta={},
                        status=StatusEnum.PENDING,
                        balance=decimal.Decimal("1.00"),
                        active=True,
                        created_at=ts1,
                    ))
                    await s.flush()
                    await sp.rollback()
                found = (await s.execute(select(Item).where(Item.name == "temp_sp"))).scalar_one_or_none()
                assert found is None

    async def a_savepoint_err():
        async with AsyncSession(engine) as s:
            async with s.begin():
                try:
                    async with s.begin_nested():
                        s.add(Item(
                            ext_id=uuid.uuid4(),
                            name="alpha",
                            tags=["dup"],
                            meta={},
                            status=StatusEnum.PENDING,
                            balance=decimal.Decimal("1.00"),
                            active=True,
                            created_at=ts1,
                        ))
                        await s.flush()
                except IntegrityError:
                    pass
                cnt = (await s.execute(select(func.count(Item.id)))).scalar_one()
                assert cnt >= 2

    async def a_array_and_agg():
        async with AsyncSession(engine) as s:
            hits = (await s.execute(select(Item).where(any_(Item.tags) == "a"))).scalars().all()
            assert len(hits) == 1 and hits[0].name == "alpha"
            cnt = (await s.execute(select(func.count(Item.id)))).scalar_one()
            assert cnt == 2, f"cnt={cnt}"

    async def a_reflect():
        async with engine.connect() as conn:
            await conn.run_sync(run_reflection_checks)

    await step("ddl", a_ddl)
    await step("core_upsert_returning", a_core_upsert)
    await step("orm_crud_types", a_orm_crud)
    await step("savepoint_rollback", a_savepoint_ok)
    await step("savepoint_after_error", a_savepoint_err)
    await step("array_any_and_agg", a_array_and_agg)
    await step("inspect_reflection", a_reflect)
    await engine.dispose()
    return passed


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_ready(port: int, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"pgsqlite did not listen on port {port} within {timeout}s")


def main() -> int:
    parser = argparse.ArgumentParser(description="SQLAlchemy 2.x multi-driver compatibility tests for pgsqlite")
    parser.add_argument("--port", type=int, help="Existing pgsqlite port; if omitted, spawns a temporary server")
    parser.add_argument("--binary", default=os.path.join(REPO_ROOT, "target", "release", "pgsqlite"))
    args = parser.parse_args()

    proc = None
    tmpdir = None
    port = args.port
    try:
        if port is None:
            port = _free_port()
            tmpdir = tempfile.TemporaryDirectory(prefix="pgsqlite_sa2x_")
            db_path = os.path.join(tmpdir.name, "test.db")
            proc = subprocess.Popen(
                [args.binary, "-p", str(port), "-d", db_path, "--log-level", "error"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            _wait_ready(port)

        results = {}
        results["psycopg2"] = check_sync_driver("psycopg2", f"postgresql+psycopg2://postgres:postgres@127.0.0.1:{port}/main")
        results["psycopg3-text"] = check_sync_driver("psycopg3-text", f"postgresql+psycopg://postgres:postgres@127.0.0.1:{port}/main")
        results["psycopg3-binary"] = check_sync_driver(
            "psycopg3-binary",
            f"postgresql+psycopg://postgres:postgres@127.0.0.1:{port}/main",
            connect_args={"options": "-c binary=1"},
        )
        results["asyncpg"] = asyncio.run(check_asyncpg(f"postgresql+asyncpg://postgres:postgres@127.0.0.1:{port}/main"))

        print("\n=== SUMMARY ===")
        failed = 0
        for drv, res in results.items():
            ok = sum(1 for v in res.values() if v == "PASS")
            tot = len(res)
            print(f"  {drv}: {ok}/{tot}")
            for k, v in res.items():
                if v != "PASS":
                    failed += 1
                    print(f"    -> {k}: {v}")
        return 1 if failed else 0
    finally:
        if proc is not None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        if tmpdir is not None:
            tmpdir.cleanup()


if __name__ == "__main__":
    sys.exit(main())
