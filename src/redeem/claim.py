import os
import asyncio
import aiohttp
import websockets
import json
from datetime import datetime, timezone
from dateutil import parser
from typing import Optional, Dict, Any, List, Tuple
from web3 import Web3
from dotenv import load_dotenv
from py_builder_relayer_client.client import RelayClient
from py_builder_relayer_client.models import SafeTransaction, OperationType
from py_builder_signing_sdk.config import BuilderConfig
from py_builder_signing_sdk.sdk_types import BuilderApiKeyCreds
from src.redeem.constants import CTF_EXCHANGE_ADDRESS, USDC_ADDRESS, CTF_ABI, RELAYER_URL, CLOB_WS_URL


load_dotenv()


# Константы
GAMMA_API_URL = "https://gamma-api.polymarket.com/events"
COIN_SLUGS = {
    "1": ("BTC", "btc-updown-15m"),
    "2": ("ETH", "eth-updown-15m"),
    "3": ("SOL", "sol-updown-15m"),
    "4": ("XRP", "xrp-updown-15m"),
}


class AutoClaim:
    def __init__(self):
        # Инициализация ClaimManager
        self.pk = os.getenv("POLYGON_PK") or ""
        self.api_key = os.getenv("POLY_API_KEY") or ""
        self.api_secret = os.getenv("POLY_API_SECRET") or ""
        self.api_passphrase = os.getenv("POLY_API_PASSPHRASE") or ""

        # Инициализация Relayer Client
        creds = BuilderApiKeyCreds(
            key=self.api_key,
            secret=self.api_secret,
            passphrase=self.api_passphrase
        )
        config = BuilderConfig(local_builder_creds=creds)

        self.client = RelayClient(
            relayer_url=RELAYER_URL,
            chain_id=137,
            private_key=self.pk,
            builder_config=config
        )

        # Web3 для кодирования данных
        self.w3 = Web3()
        self.ctf_address = Web3.to_checksum_address(CTF_EXCHANGE_ADDRESS)
        self.usdc_address = Web3.to_checksum_address(USDC_ADDRESS)
        self.contract = self.w3.eth.contract(address=self.ctf_address, abi=CTF_ABI)

    async def find_next_event(self, slug_prefix: str, min_minutes: float = 0, max_minutes: float = 15) -> Optional[Dict[str, Any]]:
        """
        Сканирует рынки и находит ближайшее событие в заданном временном окне.
        Возвращает полные данные события или None.
        """
        print(f"🔍 Поиск события {slug_prefix} (Окно: {min_minutes}-{max_minutes} мин)")

        while True:
            try:
                events = await self._fetch_active_markets()
                event = self._filter_events(events, slug_prefix, min_minutes, max_minutes)

                if event:
                    return event

                await asyncio.sleep(5)

            except Exception as e:
                print(f"❌ Ошибка сканера: {e}")
                await asyncio.sleep(5)

    async def _fetch_active_markets(self) -> List[Dict[str, Any]]:
        """Получает список активных рынков через Gamma API"""
        params = {
            "active": "true",
            "closed": "false",
            "limit": "500",
            "order": "endDate",
            "ascending": "true"
        }

        async with aiohttp.ClientSession() as session:
            async with session.get(GAMMA_API_URL, params=params) as response:
                response.raise_for_status()
                return await response.json()

    def _filter_events(
        self,
        events: List[Dict[str, Any]],
        target_prefix: str,
        min_m: float,
        max_m: float
    ) -> Optional[Dict[str, Any]]:
        """Фильтрует события по slug prefix и временному окну"""
        now = datetime.now(timezone.utc)

        for event in events:
            slug = event.get("slug", "")

            if not slug.startswith(target_prefix):
                continue

            end_date_str = event.get("endDate")
            if not end_date_str:
                continue

            try:
                end_dt = parser.isoparse(end_date_str)
                minutes_left = (end_dt - now).total_seconds() / 60

                if min_m <= minutes_left <= max_m:
                    print(f"✅ НАЙДЕНО: {slug} (через {minutes_left:.1f} мин)")
                    return event

            except Exception:
                continue

        return None

    async def wait_for_resolution(self, asset_ids: List[str], end_date_str: str) -> Optional[int]:
        """
        Подключается к WebSocket и ожидает сообщения market_resolved.
        При разрыве соединения автоматически переподключается.
        Возвращает индекс победителя (0 или 1) или None при ошибке.
        """
        try:
            end_dt = parser.isoparse(end_date_str)
        except Exception:
            print(f"❌ Некорректная дата окончания: {end_date_str}")
            return None

        print(f"🔌 Подключение к WebSocket для отслеживания завершения события...")
        print(f"🎫 Asset IDs: {[aid[-6:] for aid in asset_ids]}")

        # Параметры переподключения
        retry_delay = 1  # Начальная задержка в секундах
        max_retry_delay = 60  # Максимальная задержка
        attempt = 0

        while True:
            attempt += 1

            try:
                async with aiohttp.ClientSession() as session:
                    async with session.ws_connect(CLOB_WS_URL, heartbeat=30) as ws:
                        # Подписка на market events для этих asset_ids
                        subscribe_payload = {
                            "assets_ids": asset_ids,
                            "type": "market",
                            "custom_feature_enabled": True
                        }
                        await ws.send_str(json.dumps(subscribe_payload))

                        if attempt == 1:
                            print(f"✅ Подписка на события рынка успешна")
                        else:
                            print(f"✅ Переподключение успешно (попытка #{attempt})")

                        print(f"⏳ Ожидание завершения события...")

                        # Сбрасываем задержку при успешном подключении
                        retry_delay = 1

                        # Ожидаем сообщения
                        async for msg in ws:
                            try:
                                data = json.loads(msg.data)

                                # Обрабатываем как список или одиночное сообщение
                                messages = data if isinstance(data, list) else [data]

                                for message in messages:
                                    # Если в сообщении есть winning_asset_id - значит событие завершено
                                    winning_asset_id = message.get("winning_asset_id")

                                    if winning_asset_id:
                                        # Определяем индекс победителя
                                        try:
                                            winner_index = asset_ids.index(winning_asset_id)
                                            winning_outcome = message.get("winning_outcome", "Unknown")
                                            print(f"🏆 Событие завершено! Победитель: {winning_outcome} (индекс {winner_index})")
                                            return winner_index
                                        except ValueError:
                                            print(f"❌ winning_asset_id {winning_asset_id} не найден в списке asset_ids")
                                            return None

                            except (json.JSONDecodeError, AttributeError):
                                # Игнорируем сообщения, которые не являются JSON или не имеют .data
                                continue

            except Exception as e:
                print(f"⚠️ WebSocket ошибка (попытка #{attempt}): {e}")
                print(f"🔄 Переподключение через {retry_delay} сек...")

                await asyncio.sleep(retry_delay)

                # Экспоненциальное увеличение задержки
                retry_delay = min(retry_delay * 2, max_retry_delay)

                # Продолжаем цикл для повторной попытки
                continue

    async def _fetch_event_by_slug(self, slug: str) -> Optional[Dict[str, Any]]:
        """Получает данные события по slug"""
        params = {"slug": slug}

        async with aiohttp.ClientSession() as session:
            try:
                async with session.get(GAMMA_API_URL, params=params) as response:
                    response.raise_for_status()
                    data = await response.json()

                    if data and len(data) > 0:
                        return data[0]

                    return None

            except Exception as e:
                print(f"❌ Ошибка получения данных: {e}")
                return None

    async def claim_winnings(self, condition_id: str, winning_outcome_index: int) -> bool:
        """
        Отправляет транзакцию на клейм токенов.
        winning_outcome_index: 0 для YES/UP, 1 для NO/DOWN.
        """
        print(f"💰 Попытка забрать выигрыш для Condition: {condition_id[:16]}...{condition_id[-8:]}")

        try:
            # Подготовка данных
            parent_collection_id = "0x" + "0" * 64
            index_set = [1 << winning_outcome_index]

            # Кодируем вызов функции
            encoded_data = self.contract.encode_abi(
                abi_element_identifier="redeemPositions",
                args=[
                    self.usdc_address,
                    parent_collection_id,
                    condition_id,
                    index_set
                ]
            )

            # Создаем транзакцию для Relayer
            safe_tx = SafeTransaction(
                to=self.ctf_address,
                data=encoded_data,  # type: ignore
                value="0",
                operation=OperationType.Call
            )

            # Отправляем через Relayer (Gasless!)
            loop = asyncio.get_running_loop()
            response = await loop.run_in_executor(None, self.client.execute, [safe_tx])

            tx_hash = response.transaction_hash
            print(f"✅ Запрос на клейм отправлен! TX Hash: {tx_hash}")
            return True

        except Exception as e:
            print(f"❌ Ошибка при клейме: {e}")
            return False

    async def run_cycle(self, slug_prefix: str):
        """Основной цикл: поиск события -> ожидание -> клейм -> повтор"""
        iteration = 1

        while True:
            print(f"\n{'=' * 60}")
            print(f"🔄 ЦИКЛ #{iteration}")
            print(f"{'=' * 60}\n")

            # 1. Найти ближайшее событие
            event = await self.find_next_event(slug_prefix, min_minutes=0, max_minutes=15)

            if not event:
                print("⚠️ События не найдены, продолжаю поиск...")
                continue

            slug = event.get("slug", "")
            end_date = event.get("endDate", "")
            markets = event.get("markets", [])

            if not markets:
                print(f"❌ Нет данных о рынке для {slug}")
                iteration += 1
                continue

            market = markets[0]
            condition_id = market.get("conditionId", "")

            if not condition_id:
                print(f"❌ Нет condition_id для {slug}")
                iteration += 1
                continue

            # Извлекаем asset_ids (token IDs)
            tokens_json = market.get("clobTokenIds", "[]")
            try:
                asset_ids = json.loads(tokens_json)
            except:
                asset_ids = []

            if len(asset_ids) < 2:
                print(f"❌ Некорректные asset_ids для {slug}")
                iteration += 1
                continue

            print(f"📋 Событие: {slug}")
            print(f"📅 Конец: {end_date}")
            print(f"🎫 Condition ID: {condition_id[:16]}...{condition_id[-8:]}")

            # 2. Дождаться завершения и получить победителя через WebSocket
            winner_index = await self.wait_for_resolution(asset_ids, end_date)

            if winner_index is None:
                print(f"❌ Не удалось определить победителя для {slug}")
                iteration += 1
                continue

            # 3. Клеймить
            success = await self.claim_winnings(condition_id, winner_index)

            if success:
                print(f"🎉 Клейм успешно отправлен для {slug}!")
            else:
                print(f"⚠️ Не удалось отправить клейм для {slug}")

            print(f"\n✅ Цикл #{iteration} завершен. Поиск следующего события...\n")
            iteration += 1

            # Небольшая пауза перед следующим циклом
            await asyncio.sleep(2)


def show_menu():
    """Показывает меню выбора монеты"""
    print("\n" + "=" * 60)
    print("💰 POLYMARKET AUTO-CLAIM BOT")
    print("=" * 60)
    print("\nВыберите монету для отслеживания:\n")

    for key, (name, slug) in COIN_SLUGS.items():
        print(f"  {key}. {name} ({slug})")

    print("\n  0. Выход")
    print("=" * 60)


async def main():
    """Entry point"""
    while True:
        show_menu()

        choice = input("\nВведите номер: ").strip()

        if choice == "0":
            print("👋 Выход...")
            break

        if choice not in COIN_SLUGS:
            print("❌ Некорректный выбор. Попробуйте снова.")
            await asyncio.sleep(1)
            continue

        coin_name, slug_prefix = COIN_SLUGS[choice]

        print(f"\n🚀 Запущен мониторинг для {coin_name} ({slug_prefix})")
        print("⚠️ Нажмите Ctrl+C для возврата в меню\n")

        claimer = AutoClaim()

        try:
            await claimer.run_cycle(slug_prefix)
        except KeyboardInterrupt:
            print("\n\n⏸️ Остановлено пользователем. Возврат в меню...\n")
            await asyncio.sleep(1)
        except Exception as e:
            print(f"\n❌ Ошибка: {e}\n")
            await asyncio.sleep(2)


if __name__ == "__main__":
    asyncio.run(main())
